//! HWP 5.x engine (designllm-hwp): native HWP read, edit and save through rhwp 0.9, verified by
//! an independent record parser.
//!
//! - Open keeps the original bytes as the immutable asset and projects the document into the
//!   shared FlowDocument (layout/render through designllm-flow and designllm-render).
//! - Edits become a replayable log of native commands (native.preserved["hwp"]). Every candidate
//!   is replayed, serialized and checked before commit: rhwp content-loss report must be empty,
//!   unrelated records must be byte-identical (layout caches excepted), and an independent parse
//!   of the output must equal rhwp's model. Anything else is rejected (UNSAFE_ROUNDTRIP).
//! - Saving an unedited document returns the original bytes.
//! - Figure links (docs/FIGURE_LINKS.md): a png render is written as an ordinary HWP picture
//!   (BinData item + picture records). Inserted images are inline pictures in their own paragraph;
//!   a linked picture keeps anchor and width and takes the render's aspect ratio. Original BinData
//!   is never rewritten; the link itself lives in `Artifact.figure_links`.
pub mod edit;
pub mod guard;
pub mod inspect;
pub mod project;
pub mod view;

use designllm_contracts::*;
use edit::{Core, NativeEdit};
use project::{Assets, IdMap, LocKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use rhwp::model::control::{Control, FieldType};
use rhwp::model::hyperlink::command_uri;
use rhwp::model::paragraph::Paragraph as RParagraph;

pub const ENGINE_ID: &str = "designllm-hwp";
pub const MIME_HWP: &str = "application/x-hwp";
/// Persisted feature of an edit history that holds a named style change (NativeEdit::NamedStyleChar): an older build
/// cannot replay it and refuses the snapshot before reading it (R09).
pub const FEATURE_NAMED_STYLES: &str = "hwp.named_styles.v1";
const STATE_KEY: &str = "hwp";
/// rhwp's blank HWP 5 template (MIT), used for new documents.
const BLANK_TEMPLATE: &[u8] = include_bytes!("../assets/blank2010.hwp");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HwpState {
    format: String,
    version: String,
    #[serde(default)]
    edits: Vec<NativeEdit>,
    /// Paragraph paths ("s.p") where rhwp and the independent parser disagree: read-only.
    #[serde(default)]
    protected_paragraphs: Vec<String>,
    /// Whole document read-only (reason).
    #[serde(default)]
    read_only: Option<String>,
}

/// HWP 5 engine. Keeps a small cache of verified states (original hash + edit log) so core's
/// validate() calls do not replay the same edits again.
#[derive(Default)]
pub struct HwpEngine {
    verified: std::sync::Mutex<std::collections::VecDeque<String>>,
    models: std::sync::Mutex<std::collections::VecDeque<(String, rhwp::model::document::Document, guard::Plan, std::sync::Arc<inspect::Hwp5>)>>,
}

const MODEL_CACHE: usize = 4;

/// HWP losses of content on every page (headers and footers) or of a page the layout does not know (note bodies): a
/// page selection keeps them all, with the shared `page_losses::WHOLE_DOCUMENT_LOSSES`.
const WHOLE_DOCUMENT_LOSSES: [&str; 2] = ["hwp_header_footer_not_rendered", "hwp_note_not_rendered"];

/// Content that the visual export does not draw: a blocking export loss unless the request allows its code. The projection
/// gives these, so an edit that projects the document again replaces them.
fn is_unrendered_loss(code: &str) -> bool {
    WHOLE_DOCUMENT_LOSSES.contains(&code) || page_losses::WHOLE_DOCUMENT_LOSSES.contains(&code) || code == page_losses::OBJECT_NOT_RENDERED
}

const VERIFIED_CACHE: usize = 16;

fn state_key(artifact: &Artifact, state: &HwpState) -> String {
    let asset = artifact.native.as_ref().map(|n| n.asset.sha256.clone()).unwrap_or_default();
    sha256(format!("{asset}
{}", serde_json::to_string(&state.edits).unwrap_or_default()).as_bytes())
}

struct Session {
    core: Core,
    plan: guard::Plan,
    original: std::sync::Arc<inspect::Hwp5>,
    ids: IdMap,
    body: FlowDocument,
    state: HwpState,
}

fn state_of(artifact: &Artifact) -> DltResult<HwpState> {
    let native = artifact.native.as_ref().ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "HWP artifact has no native source"))?;
    match native.preserved.get(STATE_KEY) {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| DltError::new(ErrorCode::IntegrityError, format!("HWP edit state is invalid: {e}"))),
        None => Err(DltError::new(ErrorCode::IntegrityError, "HWP edit state is missing")),
    }
}

fn flow(artifact: &Artifact) -> DltResult<&FlowDocument> {
    match &artifact.body { ArtifactBody::Flow(f) => Ok(f), _ => Err(DltError::invalid("HWP artifact body must be a flow document")) }
}

fn diag(code: &str, message: impl Into<String>, blocking: bool) -> Diagnostic { Diagnostic { code: code.into(), message: message.into(), node_id: None, blocking, params: Default::default() } }

fn reject_security_header(header: &inspect::FileHeaderInfo, has_signature_stream: bool) -> DltResult<()> {
    let mut diagnostics = Vec::new();
    let protected: Vec<&str> = [
        (header.encrypted(), "password-encryption flag bit 1"),
        (header.distribution(), "distribution/read-only flag bit 2"),
        (header.drm(), "DRM flag bit 4"),
        (header.certificate_encrypted(), "certificate-encryption flag bit 8"),
        (header.certificate_drm(), "certificate-DRM flag bit 10"),
        (header.personal_info_security(), "personal-information security flag bit 13"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect();
    if !protected.is_empty() {
        diagnostics.push(diag(
            "hwp.protected_document_unsupported",
            format!("Protected HWP markers are not supported: {}", protected.join(", ")),
            true,
        ));
    }
    let signatures: Vec<&str> = [
        (header.digital_signature(), "digital-signature flag bit 7"),
        (header.signature_reserved(), "signature-reserved flag bit 9"),
        (has_signature_stream, "/DocOptions/DigitalSignature stream"),
    ]
    .into_iter()
    .filter_map(|(present, name)| present.then_some(name))
    .collect();
    if !signatures.is_empty() {
        diagnostics.push(diag(
            "hwp.digital_signature_unsupported",
            format!("Signed HWP markers are not supported: {}", signatures.join(", ")),
            true,
        ));
    }
    if diagnostics.is_empty() { return Ok(()); }
    let message = diagnostics.iter().map(|d| d.message.as_str()).collect::<Vec<_>>().join("; ");
    let mut error = DltError::unsupported(message);
    error.diagnostics = diagnostics;
    Err(error)
}

fn unsafe_roundtrip(diagnostics: Vec<Diagnostic>) -> DltError {
    let summary = diagnostics.iter().filter(|d| d.blocking).map(|d| d.message.clone()).take(3).collect::<Vec<_>>().join("; ");
    let mut e = DltError::new(ErrorCode::UnsafeRoundtrip, format!("The HWP save could not be verified: {summary}"));
    e.diagnostics = diagnostics;
    e
}

fn support(read: Support, render: Support, edit: Support, preserve: Support, write: Support, reason: &str) -> FeatureSupport {
    FeatureSupport { read, render, edit, preserve, write, reason: reason.into() }
}

fn capability_report(read_only: bool) -> CapabilityReport {
    use Support::*;
    let e = |s: Support| if read_only { PreserveOnly } else { s };
    let mut c = BTreeMap::new();
    c.insert(Feature::Text, support(Supported, Approximate, e(Supported), Supported, Supported, "Body and one-level table cell text; no edits next to embedded objects or field markers; tabs/line breaks not inserted"));
    c.insert(Feature::TextStyle, support(Supported, Approximate, e(Supported), Supported, Supported, "font family (same face in all language slots), size, color, bold, italic"));
    c.insert(Feature::ParagraphFormat, support(Supported, Approximate, e(Supported), Supported, Supported, "alignment, spacing before/after, first-line indent, fixed line spacing; left/right paragraph margins preserved but not shown"));
    c.insert(Feature::PageSetup, support(Supported, Approximate, e(Supported), Supported, Supported, "paper size and margins (header/footer/gutter areas kept)"));
    c.insert(Feature::Tables, support(Supported, Approximate, e(Approximate), Supported, Supported, "cell text, column widths and plain-grid table insertion; merges and row/column changes not edited"));
    c.insert(Feature::Images, support(Supported, Approximate, e(Approximate), Supported, Supported, "PNG/JPEG/BMP/GIF insertion as an inline picture in its own paragraph; removal of pictures that stand alone in a paragraph; floating positions shown in text order"));
    c.insert(Feature::FigureLinks, support(Supported, Supported, e(Supported), Supported, Supported, "png renders are saved as ordinary HWP pictures (BinData + picture records); the link is kept in the workspace, not in the file; the picture keeps its anchor and width and takes the render's aspect ratio; pictures in tables, headers or groups, rotated or flipped pictures and pictures with image effects are refused"));
    c.insert(Feature::NamedStyles, support(Approximate, Approximate, e(Approximate), Supported, Supported, "Named paragraph styles support registered fonts, size, colour, bold and italic. In body text and table cells, runs of that style switch to its new character shape if they used its previous shape ID, including direct formatting that reused that ID. Other runs keep their shape. The whole edit is refused for a following run in a header, footer, footnote, endnote, text box or drawing text, caption, memo, master page or character overlap, or if the document contains an unknown control. Character styles and clear_overrides are unsupported. Runs show effective styles."));
    c.insert(Feature::NativeRoundtrip, support(Supported, Supported, e(Supported), Supported, Supported, "native HWP 5 save; unedited documents keep original bytes; edits verified by an independent record parser"));
    c.insert(Feature::RenderPdf, support(Supported, Approximate, Unsupported, Supported, Supported, "own layout, not Hancom-identical; headers/footers, notes and drawing objects are not drawn"));
    c.insert(Feature::RenderPng, support(Supported, Approximate, Unsupported, Supported, Supported, "own layout"));
    c.insert(Feature::RenderSvg, support(Supported, Approximate, Unsupported, Supported, Supported, "own layout"));
    c.insert(Feature::HitTest, support(Supported, Supported, Unsupported, Supported, Supported, "source map of the own layout"));
    c.insert(Feature::TextSelection, support(Supported, Supported, Unsupported, Supported, Supported, "grapheme ranges; inline objects count as one U+FFFC grapheme"));
    c.insert(Feature::Pages, support(Supported, Approximate, Unsupported, Supported, Supported, "pagination is computed by the own layout"));
    // Contract v1.2 (P0): refused at every entry point; nothing of it is read from or written to HWP.
    c.insert(Feature::Effects, support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "contract v1.2 corner radius, opacity, shadows, gradients and the new text and stroke fields are not supported for HWP; styles carrying them are refused"));
    c.insert(Feature::VectorPaths, support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "contract v1.2 vector paths and icons are not supported for HWP; path nodes are refused"));
    c.insert(Feature::GridLayout, support(Unsupported, Unsupported, Unsupported, Unsupported, Unsupported, "contract v1.2 grid layout and the new layout fields do not apply to HWP flow documents and are refused"));
    c
}

fn manifest(h: &inspect::Hwp5) -> PreservationManifest {
    let mut parts = BTreeMap::new();
    for (path, s) in &h.streams {
        let payload = s.decoded.as_ref().unwrap_or(&s.raw);
        let immutable = !(path == "/DocInfo" || path.starts_with("/BodyText/") || path == "/PrvText" || path == "/PrvImage");
        parts.insert(path.clone(), PartFingerprint { sha256: sha256(payload), content_type: None, immutable });
    }
    PreservationManifest { parts, opaque_regions: vec![] }
}

fn paragraph_path_key(loc: &project::NodeLoc) -> Option<String> {
    (loc.path.len() >= 2).then(|| format!("{}.{}", loc.path[0], loc.path[1]))
}

/// Paragraph-level agreement of rhwp and the independent parser on open.
fn agreement(rv: &view::DocView, iv: &view::DocView) -> (Vec<String>, Option<String>) {
    if rv.sections.len() != iv.sections.len() { return (vec![], Some("section structure differs between parsers".into())); }
    let mut protected = Vec::new();
    for (si, (a, b)) in rv.sections.iter().zip(&iv.sections).enumerate() {
        if a.paras.len() != b.paras.len() { return (vec![], Some(format!("section {si} paragraph structure differs between parsers"))); }
        if a.page != b.page { return (vec![], Some(format!("section {si} page definition differs between parsers"))); }
        for (pi, (x, y)) in a.paras.iter().zip(&b.paras).enumerate() {
            if view::para_difference(x, y).is_some() { protected.push(format!("{si}.{pi}")); }
        }
    }
    (protected, None)
}

fn preview_text(body: &FlowDocument) -> String {
    fn cell_text(blocks: &[FlowBlock], out: &mut String) {
        for b in blocks {
            match b {
                FlowBlock::Paragraph(p) => out.push_str(&designllm_flow::logical_text(p).replace('\u{FFFC}', "")),
                FlowBlock::Table(t) => for row in &t.rows { for c in row { cell_text(&c.blocks, out); } },
                _ => {}
            }
        }
    }
    let mut out = String::new();
    for s in &body.sections {
        for b in &s.blocks {
            match b {
                FlowBlock::Paragraph(p) => { out.push_str(&designllm_flow::logical_text(p).replace('\u{FFFC}', "")); out.push_str("\r\n"); }
                FlowBlock::Table(t) => { out.push('<'); for row in &t.rows { for c in row { cell_text(&c.blocks, &mut out); } } out.push_str(">\r\n"); }
                _ => {}
            }
            if out.chars().count() >= 1000 { return out.chars().take(1000).collect(); }
        }
    }
    out
}

impl HwpEngine {
    fn source_bytes(&self, artifact: &Artifact, ctx: &EngineContext<'_>) -> DltResult<Vec<u8>> {
        let native = artifact.native.as_ref().ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "HWP artifact has no native source"))?;
        let bytes = ctx.assets.read(&native.asset)?;
        if sha256(&bytes) != native.asset.sha256 { return Err(DltError::new(ErrorCode::IntegrityError, "The original HWP bytes do not match their hash")); }
        let (header, has_signature_stream) = inspect::read_file_header(&bytes)
            .map_err(|e| DltError::new(ErrorCode::IntegrityError, format!("Original HWP unreadable: {e}")))?;
        if !header.signature_ok { return Err(DltError::new(ErrorCode::IntegrityError, "The original compound file is not an HWP document")); }
        reject_security_header(&header, has_signature_stream)?;
        if header.version[0] != 5 { return Err(DltError::unsupported(format!("HWP version {} is not supported (5.x only)", header.version_string()))); }
        Ok(bytes)
    }

    fn checked_source(&self, artifact: &Artifact, ctx: &EngineContext<'_>) -> DltResult<(Vec<u8>, inspect::Hwp5)> {
        let bytes = self.source_bytes(artifact, ctx)?;
        let original = inspect::open(&bytes).map_err(|e| DltError::new(ErrorCode::IntegrityError, format!("Original HWP unreadable: {e}")))?;
        Ok((bytes, original))
    }

    fn build(&self, bytes: &[u8], title: &str, origin: &str, asset: AssetRef, ctx: &EngineContext<'_>) -> DltResult<Artifact> {
        if bytes.starts_with(b"HWP Document File V3") { return Err(DltError::unsupported("HWP 3.x documents are not supported (HWP 5.x only)")); }
        if !inspect::is_cfb(bytes) { return Err(DltError::invalid("The file is not an HWP 5.x document (no compound file header)")); }
        let (header, has_signature_stream) = inspect::read_file_header(bytes).map_err(DltError::invalid)?;
        if !header.signature_ok { return Err(DltError::invalid("The compound file is not an HWP document")); }
        reject_security_header(&header, has_signature_stream)?;
        if header.version[0] != 5 { return Err(DltError::unsupported(format!("HWP version {} is not supported (5.x only)", header.version_string()))); }
        let h = inspect::open(bytes).map_err(|e| DltError::invalid(format!("The HWP file is damaged or not HWP 5.x: {e}")))?;
        let mut diags = Vec::new();
        if let Err(e) = &h.strict_cfb { diags.push(diag("hwp_container_irregular", format!("The original container fails strict CFB checks ({e}); saved files are written as strict CFB"), false)); }
        for p in h.doc_info_consistency() { diags.push(diag("hwp_original_inconsistent", p, false)); }
        let core = edit::open_core(bytes)?;
        let rv = view::rhwp_view(core.document());
        let iv = view::inspect_view(&h).map_err(|e| DltError::invalid(format!("The HWP records cannot be read: {e}")))?;
        let (protected, read_only) = agreement(&rv, &iv);
        if let Some(reason) = &read_only { diags.push(diag("hwp_read_only", format!("Editing is disabled: {reason}"), false)); }
        if !protected.is_empty() { diags.push(diag("hwp_paragraphs_protected", format!("{} paragraph(s) read differently by the two parsers are preserved but not editable", protected.len()), false)); }
        let proj = project::project(core.document(), None, true, Assets::Store(ctx.assets))?;
        diags.extend(proj.diagnostics);
        let state = HwpState { format: "hwp5".into(), version: h.header.version_string(), edits: vec![], protected_paragraphs: protected, read_only: read_only.clone() };
        let mut artifact = Artifact::new(ArtifactKind::Hwp, title, ENGINE_ID, ArtifactBody::Flow(proj.document));
        artifact.origin = origin.into();
        let mut preserved = BTreeMap::new();
        preserved.insert(STATE_KEY.to_string(), serde_json::to_value(&state).map_err(|e| DltError::new(ErrorCode::Internal, e.to_string()))?);
        artifact.native = Some(NativeSource { asset, mappings: proj.ids.to_mappings(), preserved, preservation: manifest(&h) });
        artifact.capabilities = capability_report(read_only.is_some());
        artifact.diagnostics = diags;
        Ok(artifact)
    }

    fn session(&self, artifact: &Artifact, ctx: &EngineContext<'_>) -> DltResult<Session> { self.session_with(artifact, ctx, false) }

    /// Native model for the artifact state. With `use_cache`, a state verified earlier in this
    /// process is restored from its edited Document instead of replaying the whole log.
    fn session_with(&self, artifact: &Artifact, ctx: &EngineContext<'_>, use_cache: bool) -> DltResult<Session> {
        let native = artifact.native.as_ref().ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "HWP artifact has no native source"))?;
        let state = state_of(artifact)?;
        let (bytes, original) = self.checked_source(artifact, ctx)?;
        let original = std::sync::Arc::new(original);
        if use_cache && !state.edits.is_empty() {
            let key = state_key(artifact, &state);
            let hit = self.models.lock().ok().and_then(|m| m.iter().find(|e| e.0 == key).map(|e| (e.1.clone(), e.2.clone())));
            if let Some((doc, plan)) = hit {
                let mut core = Core::new_empty();
                core.set_document(doc);
                let ids = IdMap::from_mappings(&native.mappings)?;
                return Ok(Session { core, plan, original, ids, body: flow(artifact)?.clone(), state });
            }
        }
        let mut core = edit::open_core(&bytes)?;
        let mut plan = guard::Plan::identity(&original);
        // Mappings describe the current state; replay only rebuilds the native model and plan.
        let ids = IdMap::from_mappings(&native.mappings)?;
        for e in &state.edits { edit::apply_native(&mut core, e, ctx.assets, &mut plan)?; }
        Ok(Session { core, plan, original, ids, body: flow(artifact)?.clone(), state })
    }

    /// Serialize the session's model and verify it against the original.
    fn serialize_checked(&self, s: &mut Session, ctx: &EngineContext<'_>, previews: bool) -> DltResult<(Vec<u8>, guard::GuardReport, Vec<Diagnostic>)> {
        let mut diags = Vec::new();
        if previews {
            let text = preview_text(&s.body);
            let image = self.preview_image(s, ctx, &mut diags)?;
            let doc = s.core.document_mut();
            let old_image = doc.preview.as_ref().and_then(|p| p.image.clone());
            doc.preview = Some(rhwp::model::document::Preview {
                text: Some(text),
                image: match image { Some(data) => Some(rhwp::model::document::PreviewImage { format: rhwp::model::document::PreviewImageFormat::Png, data }), None => { if old_image.is_some() { diags.push(diag("hwp_preview_image_removed", "The stale preview thumbnail was removed", false)); } None } },
            });
            diags.push(diag("hwp_preview_regenerated", "Preview text and thumbnail were regenerated from the edited content", false));
        }
        // Images that edits added and that no picture shows any more are not written.
        let unused = edit::unused_added_bin_data(s.core.document(), s.plan.original_bin_data);
        let pruned;
        let doc = if unused > 0 {
            let mut d = s.core.document().clone();
            edit::prune_added_bin_data(&mut d, s.plan.original_bin_data);
            diags.push(diag("hwp_unused_image_dropped", format!("{unused} image(s) added by earlier edits and no longer shown are not written"), false));
            pruned = d;
            &pruned
        } else { s.core.document() };
        let serialized = rhwp::serializer::serialize_hwp_with_report(doc).map_err(|e| DltError::new(ErrorCode::UnsafeRoundtrip, format!("HWP serialization failed: {e}")))?;
        if !serialized.content_loss().is_empty() {
            diags.push(diag("hwp_content_loss", format!("The HWP writer reported content loss: {}", serialized.content_loss().to_json()), true));
        }
        let bytes = serialized.into_bytes();
        let out = match inspect::open(&bytes) {
            Ok(h) => h,
            Err(e) => { diags.push(diag("hwp_output_unreadable", format!("Saved HWP cannot be re-read: {e}"), true)); return Err(unsafe_roundtrip(diags)); }
        };
        edit::expect_named_style_paragraphs(&mut s.plan, s.core.document());
        let report = guard::check(&s.original, &out, &s.plan);
        diags.extend(report.diagnostics.iter().cloned());
        for (at, pp) in &s.plan.pictures { diags.extend(picture_problems(&out, *at, pp, ctx.assets)); }
        match view::inspect_view(&out) {
            Ok(iv) => {
                let rv = view::rhwp_view(s.core.document());
                if let Some(d) = view::first_difference(&rv, &iv) { diags.push(diag("hwp_writer_mismatch", format!("Saved records differ from the edited model: {d}"), true)); }
                match edit::open_core(&bytes) {
                    Ok(again) => { if let Some(d) = view::first_difference(&view::rhwp_view(again.document()), &iv) { diags.push(diag("hwp_reparse_mismatch", format!("Reopened HWP differs: {d}"), true)); } }
                    Err(e) => diags.push(diag("hwp_reparse_failed", format!("Saved HWP cannot be reopened: {}", e.message), true)),
                }
            }
            Err(e) => diags.push(diag("hwp_output_unreadable", format!("Saved HWP records are invalid: {e}"), true)),
        }
        if diags.iter().any(|d| d.blocking) { return Err(unsafe_roundtrip(diags)); }
        Ok((bytes, report, diags))
    }

    fn preview_image(&self, s: &Session, ctx: &EngineContext<'_>, diags: &mut Vec<Diagnostic>) -> DltResult<Option<Vec<u8>>> {
        let width_px = s.original.streams.get("/PrvImage").and_then(|p| designllm_render::image_pixel_size(&p.raw)).map(|x| x.0).unwrap_or(724);
        let scene = match designllm_flow::layout_flow(&s.body, ctx) {
            Ok(scene) => scene,
            Err(_) => return Ok(None),
        };
        // Links are not drawn in a PNG, so the thumbnail takes the page only.
        let Some(page) = scene.pages.first().cloned() else { return Ok(None) };
        let dpi = ((i64::from(width_px) * EMU_PER_INCH + page.size.width / 2) / page.size.width.max(1)).clamp(10, 300) as u32;
        let one = RenderScene { pages: vec![page], font_fingerprint: scene.font_fingerprint.clone(), diagnostics: vec![], metadata: scene.metadata.clone(), links: vec![] };
        match ctx.renderer.encode(&one, "png", &RenderProfile { dpi, transparent: false }, ctx.assets) {
            Ok(out) => Ok(Some(out.bytes)),
            Err(e) => { diags.push(diag("hwp_preview_render_failed", format!("Preview thumbnail could not be rendered: {}", e.message), false)); Ok(None) }
        }
    }

    fn check_editable(&self, state: &HwpState, ids: &IdMap, op: &Operation) -> DltResult<()> {
        if let Some(reason) = &state.read_only { return Err(DltError::unsupported(format!("This HWP document is read-only here: {reason}"))); }
        let mut targets: Vec<&str> = Vec::new();
        match op {
            Operation::ReplaceText { range, .. } | Operation::SetTextStyleRange { range, .. } => targets.push(&range.paragraph_id),
            Operation::SetParagraphFormat { paragraph_id, .. } => targets.push(paragraph_id),
            Operation::SetStyle { node_ids, .. } => targets.extend(node_ids.iter().map(|s| s.rsplit_once(".r").map(|x| x.0).unwrap_or(s))),
            Operation::SetNodeList { node_id, .. } | Operation::SetInteractions { node_id, .. } => targets.push(node_id),
            Operation::RemoveNode { node_id } | Operation::SetTableDimensions { table_id: node_id, .. } => targets.push(node_id),
            Operation::UpdateFigureLink { link } => targets.push(&link.target_node_id),
            // A named style change can reach any paragraph of the document, including one the parsers read differently.
            Operation::SetNamedStyle { .. } if !state.protected_paragraphs.is_empty() => {
                return Err(DltError::unsupported(format!("This HWP document has {} paragraph(s) read differently by the HWP parsers; a named style change could reach them, so it is not supported here", state.protected_paragraphs.len())));
            }
            _ => {}
        }
        for t in targets {
            if let Some(key) = ids.loc(t).and_then(paragraph_path_key) {
                if state.protected_paragraphs.contains(&key) { return Err(DltError::unsupported(format!("{t} is read differently by the HWP parsers and is preserved but not editable"))); }
            }
        }
        Ok(())
    }
}

/// Image asset hashes in document order (body and table cells).
fn image_hashes(doc: &FlowDocument) -> Vec<Option<String>> {
    fn walk(blocks: &[FlowBlock], out: &mut Vec<Option<String>>) {
        for b in blocks {
            match b {
                FlowBlock::Image(n) => out.push(n.asset.as_ref().map(|a| a.sha256.clone())),
                FlowBlock::Table(t) => for row in &t.rows { for c in row { walk(&c.blocks, out); } },
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    for s in &doc.sections { walk(&s.blocks, &mut out); }
    out
}

/// Each figure link names one picture of the document that shows the linked render.
fn link_diagnostics(doc: &FlowDocument, links: &[FigureLink]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for l in links {
        let node = Some(l.target_node_id.clone());
        let mut push = |code: &str, message: String| out.push(Diagnostic { code: code.into(), message, node_id: node.clone(), blocking: true, params: Default::default() });
        if !designllm_contracts::media_header::FIGURE_REPRESENTATIONS.contains(&l.representation.as_str()) { push("hwp_figure_link_representation", format!("Figure link of {} uses representation {:?}", l.target_node_id, l.representation)); }
        if !seen.insert(l.target_node_id.as_str()) { push("hwp_figure_link_duplicate", format!("Picture {} has more than one figure link", l.target_node_id)); continue; }
        match designllm_flow::find_node(doc, &l.target_node_id) {
            Some(designllm_flow::FlowNode::Image(n)) => {
                if n.asset.as_ref().map(|a| a.sha256.as_str()) != Some(l.render_hash.as_str()) { push("hwp_figure_link_picture", format!("Picture {} does not show render {}", l.target_node_id, l.render_hash)); }
            }
            _ => push("hwp_figure_link_target", format!("Figure link target {} is not a picture of this document", l.target_node_id)),
        }
    }
    out
}

/// Saved picture of `plan` at (section, paragraph, control) against the image it must show:
/// payload bytes, crop and extent over the whole image, fresh geometry, inline placement.
fn picture_problems(out: &inspect::Hwp5, at: (usize, usize, usize), plan: &guard::PicturePlan, assets: &dyn AssetStore) -> Vec<Diagnostic> {
    let (s, p, c) = at;
    let fail = |code: &str, message: String| vec![diag(code, format!("Section {s} paragraph {p} control {c}: {message}"), true)];
    let rec = match out.picture_at(s, p, c) { Ok(r) => r, Err(e) => return fail("hwp_picture_missing", e) };
    let bytes = match assets.read(&plan.asset) { Ok(b) => b, Err(e) => return fail("hwp_picture_asset_unreadable", e.message) };
    let payload = match out.bin_data_payload(rec.bin_id) { Ok(b) => b, Err(e) => return fail("hwp_picture_payload_unreadable", e) };
    if payload != bytes || payload.len() as u64 != plan.asset.bytes || sha256(&payload) != plan.asset.sha256 { return fail("hwp_picture_payload_mismatch", format!("BinData {} holds {} bytes, not the {} bytes of image {}", rec.bin_id, payload.len(), plan.asset.bytes, plan.asset.sha256)); }
    let Some((pw, ph)) = designllm_render::image_pixel_size(&bytes) else { return fail("hwp_picture_asset_unreadable", "image size unreadable".into()) };
    let dim = (pw * 75, ph * 75);
    let mut d = Vec::new();
    if rec.crop != [0, 0, dim.0 as i32, dim.1 as i32] || rec.dims.is_some_and(|x| x != dim) { d.extend(fail("hwp_picture_crop_mismatch", format!("crop {:?} / extent {:?} do not cover the {pw}x{ph} px image", rec.crop, rec.dims))); }
    let (w, h) = (rec.width, rec.height);
    let identity = rec.matrices.iter().all(|m| *m == [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    if rec.original != (w, h) || rec.current != (w, h) || rec.rotation != 0 || !identity || rec.points != [0, 0, w as i32, 0, w as i32, h as i32, 0, h as i32] {
        d.extend(fail("hwp_picture_geometry_inconsistent", format!("frame {w}x{h}, original {:?}, current {:?}, rotation {}, points {:?}, identity matrices {identity}", rec.original, rec.current, rec.rotation, rec.points)));
    }
    if plan.inserted && rec.attr & 1 == 0 { d.extend(fail("hwp_picture_not_inline", "the inserted picture is not treated as a character".into())); }
    d
}

fn shift_ids_for(ids: &mut IdMap, e: &NativeEdit, before: &[usize], core: &Core) {
    let after: Vec<usize> = core.document().sections.iter().map(|s| s.paragraphs.len()).collect();
    match e {
        NativeEdit::InsertParagraph { section, para } | NativeEdit::InsertTable { section, para, .. } | NativeEdit::InsertPicture { section, para, .. } => {
            let delta = after[*section] as isize - before[*section] as isize;
            ids.shift_paragraphs(*section, *para, delta, None);
        }
        NativeEdit::DeleteParagraph { section, para } => ids.shift_paragraphs(*section, *para + 1, -1, Some(*para)),
        _ => {}
    }
}

fn changed_between(old: &FlowDocument, new: &FlowDocument) -> Vec<Id> {
    fn index(doc: &FlowDocument) -> BTreeMap<Id, String> {
        let mut m = BTreeMap::new();
        fn walk(blocks: &[FlowBlock], m: &mut BTreeMap<Id, String>) {
            for b in blocks {
                m.insert(designllm_flow::tree::block_id(b).to_string(), serde_json::to_string(b).unwrap_or_default());
                if let FlowBlock::Paragraph(p) = b { for r in &p.runs { m.insert(r.id.clone(), serde_json::to_string(r).unwrap_or_default()); } }
                if let FlowBlock::Table(t) = b { for row in &t.rows { for c in row { m.insert(c.id.clone(), serde_json::to_string(c).unwrap_or_default()); walk(&c.blocks, m); } } }
            }
        }
        for s in &doc.sections { m.insert(s.id.clone(), serde_json::to_string(&s.setup).unwrap_or_default()); walk(&s.blocks, &mut m); }
        m
    }
    let (a, b) = (index(old), index(new));
    let keys: BTreeSet<&Id> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

impl EngineAdapter for HwpEngine {
    fn id(&self) -> &'static str { ENGINE_ID }
    fn kinds(&self) -> Vec<ArtifactKind> { vec![ArtifactKind::Hwp] }

    fn create(&self, request: &CreateRequest, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if request.kind != ArtifactKind::Hwp { return Err(DltError::invalid("HwpEngine creates HWP documents only")); }
        if let Some(t) = &request.template { if !t.is_empty() && t != "blank" { return Err(DltError::unsupported(format!("HWP template '{t}' is not available (only 'blank')"))); } }
        let asset = context.assets.put(BLANK_TEMPLATE, MIME_HWP)?;
        let mut artifact = self.build(BLANK_TEMPLATE, &request.title, "created", asset, context)?;
        if let Some(size) = request.size {
            if !size.valid() { return Err(DltError::invalid("Requested page size is not valid")); }
            let ArtifactBody::Flow(doc) = &artifact.body else { unreachable!() };
            let section = doc.sections.first().ok_or_else(|| DltError::new(ErrorCode::Internal, "Blank template has no section"))?;
            let setup = PageSetup { size, ..section.setup.clone() };
            let op = Operation::SetPageSetup { section_id: section.id.clone(), setup };
            let prepared = self.prepare_change(&artifact, &[op], context)?;
            artifact.body = prepared.artifact.body;
            artifact.native = prepared.artifact.native;
            artifact.diagnostics = prepared.artifact.diagnostics;
        }
        Ok(artifact)
    }

    fn open(&self, input: &OpenInput, context: &EngineContext<'_>) -> DltResult<Artifact> {
        if input.kind != ArtifactKind::Hwp { return Err(DltError::invalid("HwpEngine opens HWP documents only")); }
        let bytes = context.assets.read(&input.asset)?;
        if sha256(&bytes) != input.asset.sha256 { return Err(DltError::new(ErrorCode::IntegrityError, "Asset bytes do not match their hash")); }
        self.build(&bytes, &input.title, "opened", input.asset.clone(), context)
    }

    fn capabilities(&self, artifact: &Artifact) -> CapabilityReport {
        capability_report(state_of(artifact).map(|s| s.read_only.is_some()).unwrap_or(true))
    }

    fn supported_persisted_features(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from([FEATURE_NAMED_STYLES.to_string()])
    }

    fn persisted_features(&self, artifact: &Artifact) -> DltResult<std::collections::BTreeSet<String>> {
        let document = flow(artifact)?;
        let diagnostics = designllm_flow::guard::document_v12_diagnostics("HWP documents", document);
        if !diagnostics.is_empty() {
            let mut error = DltError::new(ErrorCode::SchemaUnsupported, "HWP snapshot contains extension fields this engine refuses to persist");
            error.diagnostics = diagnostics;
            return Err(error);
        }
        let mut keys = std::collections::BTreeSet::new();
        if let Ok(state) = state_of(artifact) {
            if state.edits.iter().any(|e| matches!(e, NativeEdit::NamedStyleChar { .. })) { keys.insert(FEATURE_NAMED_STYLES.to_string()); }
        }
        Ok(keys)
    }

    fn inspect(&self, artifact: &Artifact, node_ids: &[Id]) -> DltResult<Value> {
        let state = state_of(artifact)?;
        let doc = flow(artifact)?;
        let ids = IdMap::from_mappings(&artifact.native.as_ref().map(|n| n.mappings.clone()).unwrap_or_default())?;
        if node_ids.is_empty() {
            let paragraphs = { let mut n = 0; designllm_flow::tree::for_each_paragraph(doc, |_| n += 1); n };
            return Ok(json!({"format": state.format, "version": state.version, "sections": doc.sections.len(), "paragraphs": paragraphs,
                "native_edits": state.edits.len(), "read_only": state.read_only, "protected_paragraphs": state.protected_paragraphs,
                "diagnostics": artifact.diagnostics}));
        }
        let mut nodes = Vec::new();
        for id in node_ids {
            let node = designllm_flow::find_node(doc, id).ok_or_else(|| { let mut e = DltError::new(ErrorCode::NotFound, format!("Node {id} not found")); e.affected_ids = vec![id.clone()]; e })?;
            let value = match node {
                designllm_flow::FlowNode::Paragraph(p) => json!({"id": id, "type": "paragraph", "text": designllm_flow::logical_text(p), "graphemes": designllm_flow::grapheme_count(p), "runs": p.runs, "format": p.format}),
                designllm_flow::FlowNode::Run { paragraph, index } => json!({"id": id, "type": "run", "paragraph_id": paragraph.id, "run": paragraph.runs[index], "range": designllm_flow::LogicalParagraph::new(paragraph).run_range(index)}),
                designllm_flow::FlowNode::Table(t) => json!({"id": id, "type": "table", "column_widths": t.column_widths, "rows": t.rows.len()}),
                designllm_flow::FlowNode::Cell { cell, row, .. } => json!({"id": id, "type": "cell", "row": row, "column_span": cell.column_span, "row_span": cell.row_span}),
                designllm_flow::FlowNode::Image(n) => json!({"id": id, "type": "image", "bounds": n.bounds, "asset": n.asset}),
                designllm_flow::FlowNode::Opaque { label, .. } => json!({"id": id, "type": "opaque", "label": label, "editable": false}),
                designllm_flow::FlowNode::Section(s) => json!({"id": id, "type": "section", "setup": s.setup, "blocks": s.blocks.len()}),
            };
            let mut value = value;
            if let Some(loc) = ids.loc(id) { value["native"] = json!(loc); }
            nodes.push(value);
        }
        Ok(json!({"nodes": nodes}))
    }

    fn validate_read_source(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<()> {
        self.source_bytes(artifact, context).map(|_| ())
    }

    fn prepare_change(&self, artifact: &Artifact, operations: &[Operation], context: &EngineContext<'_>) -> DltResult<PreparedChange> {
        // Contract v1.2 content is refused by name before anything is read or changed (P0, R09).
        designllm_flow::guard::refuse_v12_operations("HWP documents", operations)?;
        match self.prepare_inner(artifact, operations, context, true) {
            Err(e) if e.code == ErrorCode::UnsafeRoundtrip => self.prepare_inner(artifact, operations, context, false),
            other => other,
        }
    }

    fn validate(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic> { self.validate_inner(artifact, context) }

    /// Flow content (text, atoms, structure, image bytes) plus the figure links: a link is a
    /// relation of the document (CONTRACTS.md §8), so changing it is a content change. Without
    /// links the digest is the flow digest itself (unlinked documents keep their digest).
    fn content_digest(&self, artifact: &Artifact) -> DltResult<ContentDigest> {
        let d = designllm_flow::content_digest(flow(artifact)?)?;
        if artifact.figure_links.is_empty() { return Ok(d); }
        let mut links = artifact.figure_links.clone();
        links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
        Ok(ContentDigest { content: json_hash(&json!({"flow": d.content, "figure_links": links}))?, data: d.data })
    }

    fn layout(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> { self.layout_inner(artifact, context) }

    fn encode_native(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> { self.encode_native_inner(artifact, context) }

    fn verify_saved(&self, artifact: &Artifact, bytes: &[u8], context: &EngineContext<'_>) -> DltResult<PreservationVerification> { self.verify_saved_inner(artifact, bytes, context) }

    fn export(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput> { self.export_inner(artifact, request, context) }
}

impl HwpEngine {
    fn prepare_inner(&self, artifact: &Artifact, operations: &[Operation], context: &EngineContext<'_>, use_cache: bool) -> DltResult<PreparedChange> {
        if operations.is_empty() { return Err(DltError::invalid("No operations")); }
        let mut s = self.session_with(artifact, context, use_cache)?;
        let mut diags = Vec::new();
        let mut changed: Vec<Id> = Vec::new();
        let old_body = s.body.clone();
        let mut links = artifact.figure_links.clone();
        // The diagnostics of the last projection, when an operation projected the document again.
        let mut projected: Option<Vec<Diagnostic>> = None;
        for (k, op) in operations.iter().enumerate() {
            self.check_editable(&s.state, &s.ids, op)?;
            let tr = edit::translate(op, &s.body, &s.ids, &mut s.core, context.assets).map_err(|mut e| { e.message = format!("operation {k}: {}", e.message); e })?;
            if let Operation::UpdateFigureLink { link } = op {
                links.retain(|l| l.target_node_id != link.target_node_id);
                links.push(link.clone());
                if !changed.contains(&link.target_node_id) { changed.push(link.target_node_id.clone()); }
            }
            for e in &tr.edits {
                let counts: Vec<usize> = s.core.document().sections.iter().map(|x| x.paragraphs.len()).collect();
                let before = view::rhwp_view(s.core.document());
                edit::apply_native(&mut s.core, e, context.assets, &mut s.plan)?;
                shift_ids_for(&mut s.ids, e, &counts, &s.core);
                let after = view::rhwp_view(s.core.document());
                edit::check_intent(e, &before, &after, s.core.document(), &s.plan)?;
                s.state.edits.push(e.clone());
            }
            for (id, loc) in tr.new_ids { changed.push(id.clone()); s.ids.insert(id, loc); }
            // The section of an inserted block changes its block list (`changed_between` keys a section by its setup only).
            if let Operation::InsertFlowBlock { section_id, .. } = op { if !changed.contains(section_id) { changed.push(section_id.clone()); } }
            let proj = project::project(s.core.document(), Some(&s.ids), false, Assets::Store(context.assets))?;
            s.body = proj.document;
            s.ids = proj.ids;
            projected = Some(proj.diagnostics);
            diags.extend(tr.diagnostics);
        }
        // A removed picture takes its figure link with it.
        links.retain(|l| {
            let keep = matches!(designllm_flow::find_node(&s.body, &l.target_node_id), Some(designllm_flow::FlowNode::Image(_)));
            if !keep {
                diags.push(Diagnostic { code: "hwp_figure_link_removed".into(), message: format!("The figure link of removed picture {} was removed", l.target_node_id), node_id: Some(l.target_node_id.clone()), blocking: false, params: Default::default() });
                if !changed.contains(&l.target_node_id) { changed.push(l.target_node_id.clone()); }
            }
            keep
        });
        links.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));
        let (_bytes, report, mut check_diags) = self.serialize_checked(&mut s, context, false)?;
        check_diags.retain(|d| d.code != "hwp_preview_regenerated");
        diags.extend(check_diags);
        diags.push(Diagnostic { code: "hwp_save_verified".into(), message: format!("Candidate HWP verified: {} parts, {} paragraph regions compared", report.checked_parts, report.checked_regions), node_id: None, blocking: false, params: Default::default() });
        for id in changed_between(&old_body, &s.body) { if !changed.contains(&id) { changed.push(id); } }
        let mut next = artifact.clone();
        next.body = ArtifactBody::Flow(s.body);
        next.figure_links = links;
        let native = next.native.as_mut().ok_or_else(|| DltError::new(ErrorCode::Internal, "native source vanished"))?;
        native.mappings = s.ids.to_mappings();
        native.preserved.insert(STATE_KEY.into(), serde_json::to_value(&s.state).map_err(|e| DltError::new(ErrorCode::Internal, e.to_string()))?);
        // Without case: an artifact stored before the code was upper case carries `native_unit_rounding`.
        let rounding = |code: &str| code.get(..diag_codes::NATIVE_UNIT_ROUNDING.len()).is_some_and(|head| head.eq_ignore_ascii_case(diag_codes::NATIVE_UNIT_ROUNDING));
        next.diagnostics = artifact.diagnostics.iter().filter(|d| !rounding(&d.code)).cloned().collect();
        // The unrendered-content losses describe the document as it is now: those of the last projection replace the
        // ones the artifact had (a removed drawing object takes its loss with it).
        if let Some(projected) = projected {
            next.diagnostics.retain(|d| !is_unrendered_loss(&d.code));
            next.diagnostics.extend(projected.into_iter().filter(|d| is_unrendered_loss(&d.code)));
        }
        let key = state_key(&next, &s.state);
        self.remember(key.clone());
        if let Ok(mut m) = self.models.lock() {
            m.retain(|e| e.0 != key);
            // Save verification reads these from the current model, including any later cell edits.
            s.plan.expected_runs.clear();
            s.plan.expected_cells.clear();
            m.push_back((key, s.core.document().clone(), s.plan, s.original));
            while m.len() > MODEL_CACHE { m.pop_front(); }
        }
        Ok(PreparedChange { artifact: next, changed_ids: changed, diagnostics: diags })
    }

    fn validate_inner(&self, artifact: &Artifact, context: &EngineContext<'_>) -> Vec<Diagnostic> {
        let mut out = match flow(artifact) { Ok(doc) => designllm_flow::validate_document(doc), Err(e) => return vec![diag("hwp_invalid_body", e.message, true)] };
        if let Ok(doc) = flow(artifact) { out.extend(link_diagnostics(doc, &artifact.figure_links)); }
        if let Err(e) = self.validate_read_source(artifact, context) {
            if e.diagnostics.is_empty() { out.push(diag("hwp_source_unverified", e.message, true)); }
            else { out.extend(e.diagnostics); }
            return out;
        }
        match state_of(artifact) {
            Ok(state) if !state.edits.is_empty() && self.is_verified(&state_key(artifact, &state)) => out.push(diag("hwp_save_verified", "This edit state was verified when it was prepared", false)),
            Ok(state) if !state.edits.is_empty() => match self.session(artifact, context).and_then(|mut s| self.serialize_checked(&mut s, context, false)) {
                Ok((_, report, _)) => { self.remember(state_key(artifact, &state)); out.push(diag("hwp_save_verified", format!("{} parts and {} paragraph regions verified", report.checked_parts, report.checked_regions), false)) }
                Err(e) => { out.extend(e.diagnostics.clone()); if !out.iter().any(|d| d.blocking) { out.push(diag("hwp_save_unverified", e.message, true)); } }
            },
            Ok(_) => {}
            Err(e) => out.push(diag("hwp_state_invalid", e.message, true)),
        }
        out
    }

    /// The hyperlinks of the body paragraphs (see [`paragraph_link_spans`]). The native document is read with the edits
    /// of the artifact applied, so that its paragraphs are the ones the flow holds. Paragraphs of table cells are not read.
    fn text_link_spans(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<Vec<designllm_flow::TextLinkSpan>> {
        let session = self.session(artifact, context)?;
        let document = session.core.document();
        let mut paragraphs = Vec::new();
        designllm_flow::tree::for_each_paragraph(&session.body, |p| paragraphs.push(p));
        let mut spans = Vec::new();
        for p in paragraphs {
            let Some(loc) = session.ids.loc(&p.id) else { continue };
            let [section, index] = loc.path[..] else { continue };
            if !matches!(loc.kind, LocKind::Paragraph) {
                continue;
            }
            let Some(native) = document.sections.get(section).and_then(|s| s.paragraphs.get(index)) else { continue };
            spans.extend(paragraph_link_spans(p, native));
        }
        Ok(spans)
    }

    fn layout_inner(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<RenderScene> {
        self.validate_read_source(artifact, context)?;
        let links = self.text_link_spans(artifact, context)?;
        let mut scene = designllm_flow::layout_flow_with_links(flow(artifact)?, &links, context)?;
        let mut seen: BTreeSet<(String, Option<String>, String)> = scene.diagnostics.iter().map(|d| (d.code.clone(), d.node_id.clone(), d.message.clone())).collect();
        for d in &artifact.diagnostics {
            if (d.code.starts_with("hwp_") || is_unrendered_loss(&d.code)) && seen.insert((d.code.clone(), d.node_id.clone(), d.message.clone())) { scene.diagnostics.push(d.clone()); }
        }
        scene.metadata.insert("source_format".into(), json!("hwp5"));
        Ok(scene)
    }

    fn encode_native_inner(&self, artifact: &Artifact, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        // The writer stores none of the v1.2 values, so a body that carries one is refused instead of saved without it.
        let found = designllm_flow::guard::document_v12(flow(artifact)?).into_iter().map(|f| (None, f)).collect();
        designllm_flow::guard::refuse_v12("HWP documents", found)?;
        let state = state_of(artifact)?;
        if state.edits.is_empty() {
            let bytes = self.source_bytes(artifact, context)?;
            return Ok(EncodedOutput { bytes, mime: MIME_HWP.into(), editability: Editability::NativeData, diagnostics: vec![diag("hwp_original_bytes", "No edits: the original file bytes are written unchanged", false)] });
        }
        let mut s = self.session(artifact, context)?;
        match self.serialize_checked(&mut s, context, true) {
            Ok((bytes, _, diags)) => Ok(EncodedOutput { bytes, mime: MIME_HWP.into(), editability: Editability::NativeData, diagnostics: diags }),
            Err(e) => Ok(EncodedOutput { bytes: vec![], mime: MIME_HWP.into(), editability: Editability::NativeData, diagnostics: if e.diagnostics.is_empty() { vec![diag("hwp_save_failed", e.message, true)] } else { e.diagnostics } }),
        }
    }

    fn verify_saved_inner(&self, artifact: &Artifact, bytes: &[u8], context: &EngineContext<'_>) -> DltResult<PreservationVerification> {
        let state = state_of(artifact)?;
        let original = self.source_bytes(artifact, context)?;
        if bytes == original.as_slice() {
            let h = inspect::open(bytes).map_err(|e| DltError::invalid(format!("Original HWP unreadable: {e}")))?;
            let paragraphs: usize = h.section_paths().iter().map(|p| h.records[p].iter().filter(|r| r.level == 0 && r.tag == inspect::TAG_PARA_HEADER).count()).sum();
            let diagnostics = if state.edits.is_empty() { vec![] } else { vec![diag("hwp_edits_missing", format!("The file is the unedited original but the document has {} native edits", state.edits.len()), true)] };
            return Ok(PreservationVerification { checked_parts: h.streams.len() as u32, checked_regions: paragraphs as u32, diagnostics });
        }
        let mut s = self.session(artifact, context)?;
        let mut diagnostics = Vec::new();
        let out = match inspect::open(bytes) {
            Ok(h) => h,
            Err(e) => return Ok(PreservationVerification { checked_parts: 0, checked_regions: 0, diagnostics: vec![diag("hwp_output_unreadable", format!("Saved HWP cannot be read: {e}"), true)] }),
        };
        edit::expect_named_style_paragraphs(&mut s.plan, s.core.document());
        let report = guard::check(&s.original, &out, &s.plan);
        diagnostics.extend(report.diagnostics);
        for (at, pp) in &s.plan.pictures { diagnostics.extend(picture_problems(&out, *at, pp, context.assets)); }
        // Every linked picture of the saved file holds exactly the linked render.
        for l in &artifact.figure_links {
            let path = s.ids.loc(&l.target_node_id).map(|x| x.path.clone()).unwrap_or_default();
            let problem = match path.as_slice() {
                [sec, para, ctl] => match out.picture_at(*sec, *para, *ctl) {
                    Ok(rec) => match (out.bin_data_payload(rec.bin_id), context.assets.lookup(&l.render_hash).and_then(|a| context.assets.read(&a))) {
                        (Ok(saved), Ok(render)) if saved == render && sha256(&saved) == l.render_hash => None,
                        (Ok(saved), _) => Some(format!("saved picture holds {} bytes that are not render {}", saved.len(), l.render_hash)),
                        (Err(e), _) => Some(e),
                    },
                    Err(e) => Some(e),
                },
                _ => Some("the picture has no body-text location".into()),
            };
            if let Some(p) = problem { diagnostics.push(Diagnostic { code: "hwp_saved_figure_mismatch".into(), message: format!("Figure link of {}: {p}", l.target_node_id), node_id: Some(l.target_node_id.clone()), blocking: true, params: Default::default() }); }
        }
        match view::inspect_view(&out) {
            Ok(iv) => {
                if let Some(d) = view::first_difference(&view::rhwp_view(s.core.document()), &iv) { diagnostics.push(diag("hwp_saved_content_mismatch", format!("Saved content differs from the document: {d}"), true)); }
                match edit::open_core(bytes) {
                    Ok(again) => {
                        if let Some(d) = view::first_difference(&view::rhwp_view(again.document()), &iv) { diagnostics.push(diag("hwp_reparse_mismatch", format!("Reopened HWP differs: {d}"), true)); }
                        match project::project(again.document(), None, true, Assets::HashOnly) {
                            Ok(p) => {
                                let want = designllm_flow::content_digest(flow(artifact)?)?;
                                let got = designllm_flow::content_digest(&p.document)?;
                                if want.data != got.data { diagnostics.push(diag("hwp_saved_data_mismatch", "Saved data digest differs", true)); }
                                // The flow digest ignores node ids, so the reopened file (fresh path ids) must match exactly.
                                if want.content != got.content { diagnostics.push(diag("hwp_saved_content_digest_mismatch", "Saved content (text, objects, table structure, images, order) differs from the document", true)); }
                                let strip = |d: &FlowDocument| -> Vec<String> { let mut v = Vec::new(); designllm_flow::tree::for_each_paragraph(d, |p| v.push(designllm_flow::logical_text(p))); v };
                                if strip(&p.document) != strip(flow(artifact)?) { diagnostics.push(diag("hwp_saved_text_mismatch", "Saved paragraph text differs from the document", true)); }
                                if image_hashes(&p.document) != image_hashes(flow(artifact)?) { diagnostics.push(diag("hwp_saved_images_mismatch", "Saved pictures (order or image bytes) differ from the document", true)); }
                            }
                            Err(e) => diagnostics.push(diag("hwp_saved_projection_failed", e.message, true)),
                        }
                    }
                    Err(e) => diagnostics.push(diag("hwp_reparse_failed", format!("Saved HWP cannot be reopened: {}", e.message), true)),
                }
            }
            Err(e) => diagnostics.push(diag("hwp_output_unreadable", format!("Saved HWP records are invalid: {e}"), true)),
        }
        Ok(PreservationVerification { checked_parts: report.checked_parts, checked_regions: report.checked_regions, diagnostics })
    }

    fn export_inner(&self, artifact: &Artifact, request: &ExportRequest, context: &EngineContext<'_>) -> DltResult<EncodedOutput> {
        let format = request.format.trim().to_ascii_lowercase();
        if format == "hwp" {
            designllm_flow::page_select::refuse_page_selection("hwp", request.page_indices.as_deref())?;
            return self.encode_native(artifact, context);
        }
        // The renderer's formats (`designllm_render` encode); any other is refused before the page selection, as DOCX and HWPX do.
        if !matches!(format.as_str(), "pdf" | "svg" | "png" | "svg-outline" | "svg_outline" | "svg-outlines") {
            return Err(DltError::unsupported(format!("HWP cannot export to {}", request.format)));
        }
        let mut scene = self.layout(artifact, context)?;
        let total = scene.pages.len();
        let whole_document: Vec<&str> = WHOLE_DOCUMENT_LOSSES.iter().chain(page_losses::WHOLE_DOCUMENT_LOSSES).copied().collect();
        designllm_flow::page_select::select_export_pages(&mut scene, flow(artifact)?, &format, request.page_indices.as_deref(), &whole_document)?;
        page_losses::release_allowed(&mut scene.diagnostics, &request.allowed_losses, is_unrendered_loss);
        scene.metadata.insert("total_pages".into(), json!(total));
        let _ = LocKind::Section;
        context.renderer.encode(&scene, &format, &request.profile, context.assets)
    }
}

impl HwpEngine {
    fn remember(&self, key: String) {
        if let Ok(mut v) = self.verified.lock() {
            if !v.contains(&key) { v.push_back(key); }
            while v.len() > VERIFIED_CACHE { v.pop_front(); }
        }
    }
    fn is_verified(&self, key: &str) -> bool { self.verified.lock().map(|v| v.iter().any(|k| k == key)).unwrap_or(false) }
}

/// The HYPERLINK spans of one native paragraph, as grapheme ranges of the flow paragraph `p` built from it. A field runs
/// from its start to its end character (`FieldRange`). Each inline atom (an equation, a footnote, ...) is one U+FFFC of
/// three bytes at its control position, as `project.rs` lays it out. A paragraph whose logical text is not the flow text
/// gives no span: an edit moves the text under the native positions even when it keeps the length.
fn paragraph_link_spans(p: &Paragraph, native: &RParagraph) -> Vec<designllm_flow::TextLinkSpan> {
    let chars: Vec<char> = native.text.chars().collect();
    let atoms = project::atoms_of(native);
    // The native text with an U+FFFC before each atom is the logical text of the flow paragraph, or the spans are not used.
    let lp = designllm_flow::LogicalParagraph::new(p);
    if lp.text != project::logical_text_of(native, &atoms) {
        return Vec::new();
    }
    let positions: Vec<usize> = atoms.iter().map(|&(pos, _)| pos).collect();
    native
        .field_ranges
        .iter()
        .filter_map(|range| {
            let Some(Control::Field(field)) = native.controls.get(range.control_idx) else { return None };
            if field.field_type != FieldType::Hyperlink {
                return None;
            }
            let start = logical_offset(&chars, &positions, range.start_char_idx, true);
            // A field that starts and ends at one atom is empty, but its end does not count that atom while its start does.
            // Clamping keeps the range empty instead of inverted; `grapheme_span` gives no span for either.
            let end = logical_offset(&chars, &positions, range.end_char_idx, false).max(start);
            let (start, end) = designllm_flow::grapheme_span(&lp, start..end)?;
            Some(designllm_flow::TextLinkSpan { paragraph_id: p.id.clone(), start, end, url: command_uri(&field.command) })
        })
        .collect()
}

/// The logical text offset of native character `at`: the bytes of the characters before it, and three bytes per atom
/// before it. An atom at `at` counts as before the offset only when `after_atoms` is set (a field starts after the atoms
/// at its position and ends before them).
fn logical_offset(chars: &[char], atoms: &[usize], at: usize, after_atoms: bool) -> usize {
    let text: usize = chars[..at.min(chars.len())].iter().map(|c| c.len_utf8()).sum();
    let before = atoms.iter().filter(|&&pos| if after_atoms { pos <= at } else { pos < at }).count();
    text + designllm_flow::OBJECT_REPLACEMENT_BYTES * before
}

#[cfg(test)]
mod link_span_tests {
    use super::*;
    use designllm_contracts::{ParagraphFormat, TextRun};
    use rhwp::model::control::{AutoNumber, Field};
    use rhwp::model::paragraph::FieldRange;

    /// The paragraph "a" with an inline atom (an auto number) in front of it and a HYPERLINK field over `start..end`.
    fn native_and_flow(start: usize, end: usize) -> (Paragraph, RParagraph) {
        let mut native = RParagraph::default();
        native.text = "a".into();
        native.controls = vec![
            Control::AutoNumber(AutoNumber::default()),
            Control::Field(Field { field_type: FieldType::Hyperlink, command: "https://example.com/hwp;1;0;0;".into(), ..Default::default() }),
        ];
        native.field_ranges = vec![FieldRange { start_char_idx: start, end_char_idx: end, control_idx: 1, ..Default::default() }];
        // The flow paragraph of the same text: the atom is a protected run (one U+FFFC), then "a".
        let runs = vec![TextRun { protected: true, ..TextRun::new("p.a0", "") }, TextRun::new("p.r0", "a")];
        (Paragraph { id: "p".into(), runs, style_ref: None, format: ParagraphFormat::default() }, native)
    }

    /// A field that starts and ends at the atom gives no span. Its start counts the atom and its end does not, so the end
    /// falls before the start. The field over "a" in the same paragraph gives one span.
    #[test]
    fn an_empty_field_at_an_atom_gives_no_span() {
        let (p, native) = native_and_flow(0, 1);
        assert_eq!(paragraph_link_spans(&p, &native).len(), 1, "a field over the text gives its span");
        let (p, native) = native_and_flow(0, 0);
        assert_eq!(project::atoms_of(&native), vec![(0usize, 0usize)], "the atom sits at the start of the text");
        assert_eq!(
            designllm_flow::LogicalParagraph::new(&p).text,
            project::logical_text_of(&native, &project::atoms_of(&native)),
            "the texts agree, so the spans are computed"
        );
        assert!(paragraph_link_spans(&p, &native).is_empty(), "an empty field gives no span");
    }
}

