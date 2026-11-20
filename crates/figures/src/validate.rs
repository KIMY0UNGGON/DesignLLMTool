//! Document validation: identity, panel geometry, dataset sources, evidence, diagram provenance, and image
//! assets. The layout pass runs on every validation, so plot binding and label/connector collisions are
//! checked by the same code that draws the figure.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    AssetStore, CommonLegend, Dataset, DltError, DltResult, Diagnostic, Diagram, ErrorCode, FigureDocument, FigureImage, FigurePanel,
    Operation, PanelBody, RenderProvider, VisualReviewState, sha256,
};
use designllm_contracts::media_header;
use designllm_contracts::figure_rules::{DIAGRAM_ORIGINS, IMAGE_ROTATION_LIMIT_MILLIDEG, MODEL_PROPOSAL_ORIGIN, PAPER_EVIDENCE_ORIGIN};

use crate::decimal::Dec;
use crate::dataset;
use crate::diag::{self, code, has_blocking};
use crate::digest;
use crate::evidence;
use crate::image;
use crate::layout::{self, COMMON_LEGEND_ID, VISUAL_REVIEW_ID};

/// Diagram origin codes. The list is the contract's `DIAGRAM_ORIGINS`; this name is kept for existing references.
pub const ORIGINS_NODE: [&str; 3] = DIAGRAM_ORIGINS;
pub const IMAGE_ORIGINS: [&str; 3] = ["experimental_original", "paper_extracted", "generated_illustration"];

/// Validation result. `built` is present only when nothing blocks the figure.
pub struct Full {
    pub diagnostics: Vec<Diagnostic>,
    pub built: Option<layout::Built>,
}

/// Full validation: structure, sources, and layout. `figure_id` names the page in the scene.
pub fn full(doc: &FigureDocument, figure_id: &str, assets: &dyn AssetStore, renderer: &dyn RenderProvider) -> DltResult<Full> {
    // A refused v1.2 item stops everything: nothing is laid out, exported, or encoded from it (guard.rs).
    crate::guard::require_document(doc)?;
    let mut d: Vec<Diagnostic> = Vec::new();
    if !doc.size.valid() {
        d.push(diag::blocking(code::CANVAS_INVALID, None, "canvas size must be positive and at most 10 m"));
    }
    check_identity(doc, &mut d);
    check_panel_geometry(doc, &mut d);
    check_common_legend(doc, &mut d);
    check_visual_review(doc, &mut d);
    for ds in &doc.datasets {
        check_dataset(ds, assets, &mut d);
    }
    for ev in &doc.evidence {
        evidence::check(ev, doc, assets, &mut d);
    }
    evidence::report_unreviewed_citations(doc, &mut d);
    for p in &doc.panels {
        match &p.body {
            PanelBody::Diagram(dg) => check_diagram(doc, dg, &mut d),
            PanelBody::Image(img) => check_image(doc, p, img, assets, &mut d),
            PanelBody::Plot(_) => {}
        }
    }
    let laid = layout::build(doc, figure_id, renderer)?;
    d.extend(laid.diagnostics.iter().cloned());
    let built = if has_blocking(&d) { None } else { Some(laid) };
    Ok(Full { diagnostics: d, built })
}

/// Blocking and informational diagnostics only (no layout result).
pub fn run(doc: &FigureDocument, figure_id: &str, assets: &dyn AssetStore, renderer: &dyn RenderProvider) -> DltResult<Vec<Diagnostic>> {
    Ok(full(doc, figure_id, assets, renderer)?.diagnostics)
}

/// Blocking findings that a change adds to the document `before`, judged per dataset id against the version it replaces
/// (see `dataset::change_findings`). A dataset that the change leaves as it was adds nothing. The base of each version is the
/// document before the change, so a change must carry one version per lineage (`check_put_datasets`).
pub fn change_findings(before: &FigureDocument, after: &FigureDocument, assets: &dyn AssetStore) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for ds in &after.datasets {
        let same_id = before.datasets.iter().find(|p| p.id == ds.id);
        if same_id.is_some_and(|p| p.version_hash == ds.version_hash) {
            continue;
        }
        // A new id that derives from a dataset is judged against that dataset, so it cannot change what its parent records. The
        // parent is looked up in the document before the change, then in the change itself.
        let prev = same_id.or_else(|| {
            let parent = ds.derived_from.as_ref()?;
            before
                .datasets
                .iter()
                .find(|p| p.id == parent.dataset_id)
                .or_else(|| after.datasets.iter().find(|p| p.id == parent.dataset_id && p.id != ds.id))
        });
        let prev_source = prev.and_then(|p| source_bytes(assets, &p.original_asset_hash));
        let source = source_bytes(assets, &ds.original_asset_hash);
        d.extend(dataset::change_findings(prev, prev_source.as_deref(), ds, source.as_deref()));
    }
    // A PNG that the change brings in follows the shared rule (task 49e, D2). Images the document already held keep the rule
    // they were saved under (`image::header`), so a stored project still opens and takes other changes.
    let held: BTreeSet<&str> = before.panels.iter().filter_map(|p| match &p.body { PanelBody::Image(img) => Some(img.asset.sha256.as_str()), _ => None }).collect();
    for p in &after.panels {
        let PanelBody::Image(img) = &p.body else { continue };
        if held.contains(img.asset.sha256.as_str()) { continue; }
        let Some(bytes) = source_bytes(assets, &img.asset.sha256) else { continue };
        if bytes.starts_with(&media_header::PNG_SIGNATURE) && media_header::png_size(&bytes).is_none() {
            d.push(diag::blocking(code::IMAGE_FORMAT_UNSUPPORTED, Some(p.id.as_str()), "a new PNG image needs an IHDR chunk of length 13 and a positive size"));
        }
    }
    d
}

/// Refuses a change that puts more than one version of one dataset lineage. A put joins the lineage of the dataset that it names as
/// derived_from, so the change refuses two versions of one id, a version and its derived copy, and two copies of one parent.
/// `change_findings` judges each version against the version that the document held before the change, so two versions of one
/// lineage in one change would be judged against the wrong base. The desktop sends one PutDataset per change. The engine calls this
/// with the operations of a change before it applies them.
pub fn check_put_datasets(operations: &[Operation]) -> DltResult<()> {
    let puts: Vec<&Dataset> = operations
        .iter()
        .filter_map(|op| match op {
            Operation::PutDataset { dataset } => Some(dataset),
            _ => None,
        })
        .collect();
    // A version joins the lineage of the dataset that it derives from.
    let mut link: BTreeMap<String, String> = BTreeMap::new();
    for put in &puts {
        if let Some(parent) = &put.derived_from {
            let (own, from) = (lineage_root(&link, &put.id), lineage_root(&link, &parent.dataset_id));
            if own != from {
                link.insert(own, from);
            }
        }
    }
    let mut lineages: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for put in &puts {
        lineages.entry(lineage_root(&link, &put.id)).or_default().push(put.id.as_str());
    }
    match lineages.values().find(|ids| ids.len() > 1) {
        None => Ok(()),
        Some(ids) => Err(DltError::new(
            ErrorCode::InvalidArgument,
            format!(
                "a change may put one version of a dataset lineage, but it puts {} versions of it ({}); put them in separate changes",
                ids.len(),
                ids.join(", ")
            ),
        )),
    }
}

/// The id at the root of the lineage of `id` in `link`, where each id points to the id that it joined.
fn lineage_root(link: &BTreeMap<String, String>, id: &str) -> String {
    let mut current = id;
    while let Some(next) = link.get(current) {
        if next == current {
            break;
        }
        current = next.as_str();
    }
    current.to_owned()
}

/// The bytes of a source asset, when the store holds it.
fn source_bytes(assets: &dyn AssetStore, hash: &str) -> Option<Vec<u8>> {
    let aref = assets.lookup(hash).ok()?;
    assets.read(&aref).ok()
}

/// What is wrong with a common legend, one message per problem. Empty when the legend is valid. The engine's operation and
/// validation both use it, so a stored legend is judged by the same rules as a new one.
pub(crate) fn legend_problems(legend: &CommonLegend) -> Vec<String> {
    let mut problems = Vec::new();
    if legend.items.is_empty() {
        problems.push("the common legend needs at least one item".to_owned());
    }
    for (i, item) in legend.items.iter().enumerate() {
        if item.label.trim().is_empty() {
            problems.push(format!("common legend item {i} has no label"));
        }
        if !valid_color(&item.color) {
            problems.push(format!("common legend item {i} color {:?} must be #RRGGBB or #RRGGBBAA", item.color));
        }
    }
    problems
}

fn valid_color(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|hex| (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn check_common_legend(doc: &FigureDocument, d: &mut Vec<Diagnostic>) {
    if let Some(legend) = &doc.common_legend {
        for problem in legend_problems(legend) {
            d.push(diag::blocking(code::COMMON_LEGEND_INVALID, Some(COMMON_LEGEND_ID), problem));
        }
    }
}

/// A visual review that is not approved, or whose approval is stale, is reported here as info. Export blocks on either
/// (engine.rs LOSS_CODES), and nothing else does. An approval is current only while its digest is the figure's digest now.
fn check_visual_review(doc: &FigureDocument, d: &mut Vec<Diagnostic>) {
    let Some(review) = &doc.visual_review else { return };
    if review.state != VisualReviewState::Approved {
        d.push(diag::info(code::VISUAL_REVIEW_NOT_APPROVED, None, "the visual review of this figure is not approved"));
        return;
    }
    let current = digest::approval_digest(doc).ok();
    match (&review.approved_digest, current) {
        (Some(recorded), Some(now)) if *recorded == now => {}
        (Some(_), _) => d.push(diag::info(
            code::VISUAL_REVIEW_STALE,
            None,
            "the figure's content, data, or design changed after its visual review was approved; review it again",
        )),
        (None, _) => d.push(diag::info(
            code::VISUAL_REVIEW_STALE,
            None,
            "the visual review has no record of the figure it was approved for (an earlier file); review it again",
        )),
    }
}

fn check_identity(doc: &FigureDocument, d: &mut Vec<Diagnostic>) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut add = |id: &str, d: &mut Vec<Diagnostic>| {
        if id.trim().is_empty() || id.contains(':') {
            d.push(diag::blocking(code::ID_INVALID, None, format!("id \"{id}\" must be non-empty and must not contain ':'")));
        } else if id == COMMON_LEGEND_ID {
            d.push(diag::blocking(code::ID_INVALID, Some(id), format!("id \"{id}\" is reserved for the common legend")));
        } else if id == VISUAL_REVIEW_ID {
            d.push(diag::blocking(code::ID_INVALID, Some(id), format!("id \"{id}\" is reserved for the visual review")));
        } else if !seen.insert(id.to_owned()) {
            d.push(diag::blocking(code::ID_DUPLICATE, Some(id), format!("id \"{id}\" is used more than once")));
        }
    };
    for ds in &doc.datasets {
        add(&ds.id, d);
    }
    for ev in &doc.evidence {
        add(&ev.id, d);
    }
    for p in &doc.panels {
        add(&p.id, d);
        if let PanelBody::Diagram(dg) = &p.body {
            for n in &dg.nodes {
                add(&n.id, d);
            }
            for e in &dg.edges {
                add(&e.id, d);
            }
        }
    }
}

fn check_panel_geometry(doc: &FigureDocument, d: &mut Vec<Diagnostic>) {
    for p in &doc.panels {
        let b = p.bounds;
        if b.width <= 0 || b.height <= 0 {
            d.push(diag::blocking(code::PANEL_SIZE_INVALID, Some(p.id.as_str()), "panel size must be positive"));
        } else if b.x < 0
            || b.y < 0
            || b.x.checked_add(b.width).is_none_or(|r| r > doc.size.width)
            || b.y.checked_add(b.height).is_none_or(|r| r > doc.size.height)
        {
            d.push(diag::blocking(code::PANEL_OUTSIDE_CANVAS, Some(p.id.as_str()), "panel extends outside the canvas"));
        }
    }
    for (i, a) in doc.panels.iter().enumerate() {
        for b in doc.panels.iter().skip(i + 1) {
            let (x, y) = (a.bounds, b.bounds);
            if x.width > 0 && x.height > 0 && y.width > 0 && y.height > 0 {
                let overlap = x.x < y.x + y.width && y.x < x.x + x.width && x.y < y.y + y.height && y.y < x.y + x.height;
                if overlap {
                    d.push(diag::blocking(code::PANEL_OVERLAP, Some(a.id.as_str()), format!("panel overlaps {}", b.id)));
                }
            }
        }
    }
}

fn check_dataset(ds: &designllm_contracts::Dataset, assets: &dyn AssetStore, d: &mut Vec<Diagnostic>) {
    d.extend(dataset::check_shape(ds));
    match assets.lookup(&ds.original_asset_hash) {
        Err(_) => d.push(diag::blocking(
            code::SOURCE_ASSET_MISSING,
            Some(ds.id.as_str()),
            "the original source asset of this dataset is not in the asset store",
        )),
        Ok(aref) => match assets.read(&aref) {
            Ok(bytes) => d.extend(dataset::reconcile_source(ds, &bytes)),
            Err(e) => d.push(diag::blocking(code::SOURCE_ASSET_MISSING, Some(ds.id.as_str()), format!("source cannot be read: {}", e.message))),
        },
    }
}

fn check_diagram(doc: &FigureDocument, dg: &Diagram, d: &mut Vec<Diagnostic>) {
    let node_ids: BTreeSet<&str> = dg.nodes.iter().map(|n| n.id.as_str()).collect();
    let edge_ids: BTreeSet<&str> = dg.edges.iter().map(|e| e.id.as_str()).collect();
    let evidence_ids: BTreeSet<&str> = doc.evidence.iter().map(|e| e.id.as_str()).collect();
    for (key, refs) in &dg.evidence_ids {
        if !node_ids.contains(key.as_str()) && !edge_ids.contains(key.as_str()) {
            d.push(diag::blocking(code::EVIDENCE_MAP_KEY_UNKNOWN, Some(key.as_str()), "evidence map names an element that is not in this diagram"));
        }
        for r in refs {
            if !evidence_ids.contains(r.as_str()) {
                d.push(diag::blocking(code::EVIDENCE_NOT_FOUND, Some(key.as_str()), format!("evidence \"{r}\" not found")));
            }
        }
    }
    for n in &dg.nodes {
        let origin = dg.node_origins.get(&n.id);
        if let Some(o) = origin {
            if !ORIGINS_NODE.contains(&o.as_str()) {
                d.push(diag::blocking(code::ORIGIN_INVALID, Some(n.id.as_str()), format!("origin \"{o}\" is not one of {ORIGINS_NODE:?}")));
            }
        }
        let refs = dg.evidence_ids.get(&n.id).map(Vec::len).unwrap_or(0);
        if origin.map(String::as_str) == Some(PAPER_EVIDENCE_ORIGIN) && refs == 0 {
            d.push(diag::blocking(code::EVIDENCE_REQUIRED, Some(n.id.as_str()), "a paper_evidence element needs an evidence reference"));
        }
    }
    for e in &dg.edges {
        if !ORIGINS_NODE.contains(&e.origin.as_str()) {
            d.push(diag::blocking(code::ORIGIN_INVALID, Some(e.id.as_str()), format!("origin \"{}\" is not one of {ORIGINS_NODE:?}", e.origin)));
        }
        for r in &e.evidence_ids {
            if !evidence_ids.contains(r.as_str()) {
                d.push(diag::blocking(code::EVIDENCE_NOT_FOUND, Some(e.id.as_str()), format!("evidence \"{r}\" not found")));
            }
        }
        match e.origin.as_str() {
            PAPER_EVIDENCE_ORIGIN if e.evidence_ids.is_empty() => {
                d.push(diag::blocking(code::EVIDENCE_REQUIRED, Some(e.id.as_str()), "a paper_evidence connector needs an evidence reference"));
            }
            MODEL_PROPOSAL_ORIGIN if e.evidence_ids.is_empty() => {
                d.push(diag::info(
                    code::UNSOURCED_PROPOSAL,
                    Some(e.id.as_str()),
                    "model proposal without evidence: not presented as a source fact",
                ));
            }
            _ => {}
        }
    }
}

fn check_image(doc: &FigureDocument, panel: &FigurePanel, img: &FigureImage, assets: &dyn AssetStore, d: &mut Vec<Diagnostic>) {
    let node = Some(panel.id.as_str());
    if !IMAGE_ORIGINS.contains(&img.origin.as_str()) {
        d.push(diag::blocking(code::IMAGE_ORIGIN_INVALID, node, format!("origin \"{}\" is not one of {IMAGE_ORIGINS:?}", img.origin)));
    }
    let source_ref_exists = img.source_ref.as_ref().is_some_and(|r| doc.evidence.iter().any(|e| &e.id == r));
    if img.origin == "paper_extracted" && !source_ref_exists {
        d.push(diag::blocking(code::IMAGE_SOURCE_REF_MISSING, node, "an image extracted from a paper needs a source_ref to evidence"));
    } else if let Some(r) = &img.source_ref {
        if !source_ref_exists {
            d.push(diag::blocking(code::EVIDENCE_NOT_FOUND, node, format!("source_ref \"{r}\" is not evidence in this figure")));
        }
    }

    match assets.lookup(&img.asset.sha256) {
        Err(_) => d.push(diag::blocking(code::IMAGE_ASSET_MISSING, node, "image asset is not in the asset store")),
        Ok(aref) => match assets.read(&aref) {
            Err(e) => d.push(diag::blocking(code::IMAGE_ASSET_MISSING, node, format!("image asset cannot be read: {}", e.message))),
            Ok(bytes) => {
                if sha256(&bytes) != aref.sha256 {
                    d.push(diag::blocking(code::IMAGE_HASH_MISMATCH, node, "image bytes do not match their hash"));
                }
                match image::header(&bytes) {
                    None => d.push(diag::blocking(code::IMAGE_FORMAT_UNSUPPORTED, node, "only PNG and JPEG images are supported")),
                    Some(h) => {
                        if h.mime != aref.mime {
                            d.push(diag::blocking(code::IMAGE_FORMAT_UNSUPPORTED, node, format!("asset mime {} does not match its {} bytes", aref.mime, h.mime)));
                        }
                        if h.width != img.pixel_width || h.height != img.pixel_height {
                            d.push(diag::blocking(
                                code::IMAGE_SIZE_MISMATCH,
                                node,
                                format!("declared {}x{} px but the image is {}x{} px", img.pixel_width, img.pixel_height, h.width, h.height),
                            ));
                        }
                    }
                }
            }
        },
    }
    if let Some(c) = img.crop_pixel_rect {
        let inside = c.width > 0
            && c.height > 0
            && c.x.checked_add(c.width).is_some_and(|r| r <= img.pixel_width)
            && c.y.checked_add(c.height).is_some_and(|b| b <= img.pixel_height);
        if !inside {
            d.push(diag::blocking(code::IMAGE_CROP_INVALID, node, "crop is outside the image or empty"));
        }
    }
    if img.rotation_millidegrees.unsigned_abs() >= IMAGE_ROTATION_LIMIT_MILLIDEG.unsigned_abs() {
        d.push(diag::blocking(code::IMAGE_ROTATION_INVALID, node, "rotation must be within (-360, 360) degrees"));
    }
    if let Some(cal) = &img.calibration {
        if img.origin == "generated_illustration" {
            d.push(diag::blocking(code::GENERATED_IMAGE_CALIBRATION, node, "a generated illustration cannot carry a measurement calibration"));
        }
        let positive = |t: &str| Dec::parse(t).map(|v| v.coef > 0 && !v.neg).unwrap_or(false);
        if !positive(&cal.length) || !positive(&cal.pixel_span) || cal.unit.trim().is_empty() {
            d.push(diag::blocking(
                code::CALIBRATION_INVALID,
                node,
                "calibration needs a positive length and pixel span and a unit",
            ));
        }
        if let Some(e) = &cal.evidence_id {
            if !doc.evidence.iter().any(|x| &x.id == e) {
                d.push(diag::blocking(code::EVIDENCE_NOT_FOUND, node, format!("calibration evidence \"{e}\" not found")));
            }
        }
    }
}

/// Maps dataset ids to their current versions, for scene metadata.
pub fn dataset_versions(doc: &FigureDocument) -> BTreeMap<String, String> {
    doc.datasets.iter().map(|d| (d.id.clone(), d.version_hash.clone())).collect()
}
