//! Figure change operations. Each operation edits a copy of the document. Unsupported operations fail
//! explicitly. Nothing is skipped, and nothing is repaired silently.

use designllm_contracts::{
    CommonLegend, Dataset, Diagram, DltError, DltResult, ErrorCode, Evidence, FigureDocument, FigureImage, FigurePanel, Id, Operation,
    PanelBody, PlotSpec, Rect, SourceCoverage, VisualReview, VisualReviewState,
};

use crate::dataset;
use crate::diag::{self, code};
use crate::digest;
use crate::evidence::PDF_EVIDENCE_ORIGIN;
use crate::layout::{COMMON_LEGEND_ID, VISUAL_REVIEW_ID};
use crate::validate;

fn invalid(msg: impl Into<String>) -> DltError {
    DltError::invalid(msg)
}

/// A stale `expected` value: the element changed since the caller read it, so nothing is applied.
fn stale(msg: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::RevisionConflict, msg)
}

fn not_found(what: &str, id: &str) -> DltError {
    DltError::new(ErrorCode::NotFound, format!("{what} \"{id}\" not found"))
}

fn panel_index(doc: &FigureDocument, id: &str) -> DltResult<usize> {
    doc.panels.iter().position(|p| p.id == id).ok_or_else(|| not_found("panel", id))
}

fn plot_mut<'a>(doc: &'a mut FigureDocument, id: &str) -> DltResult<&'a mut PlotSpec> {
    let i = panel_index(doc, id)?;
    match &mut doc.panels[i].body {
        PanelBody::Plot(pl) => Ok(pl),
        _ => Err(DltError::unsupported(format!("panel \"{id}\" is not a plot"))),
    }
}

fn diagram_mut<'a>(doc: &'a mut FigureDocument, id: &str) -> DltResult<&'a mut Diagram> {
    let i = panel_index(doc, id)?;
    match &mut doc.panels[i].body {
        PanelBody::Diagram(dg) => Ok(dg),
        _ => Err(DltError::unsupported(format!("panel \"{id}\" is not a diagram"))),
    }
}

fn image_mut<'a>(doc: &'a mut FigureDocument, id: &str) -> DltResult<&'a mut FigureImage> {
    let i = panel_index(doc, id)?;
    match &mut doc.panels[i].body {
        PanelBody::Image(img) => Ok(img),
        _ => Err(DltError::unsupported(format!("panel \"{id}\" is not an image"))),
    }
}

/// The image's asset must still be the one the caller read.
fn check_image_asset(img: &FigureImage, expected_sha256: &str) -> DltResult<()> {
    if img.asset.sha256 != expected_sha256 {
        return Err(stale(format!("the image asset is {}, not the expected {expected_sha256}", img.asset.sha256)));
    }
    Ok(())
}

/// A legend as it is stored: a blank title is no title. Comparisons and storage both use this form.
fn normalized_legend(legend: &CommonLegend) -> CommonLegend {
    let mut out = legend.clone();
    out.title = out.title.filter(|title| !title.trim().is_empty());
    out
}

/// A visual review as callers compare it: a blank note is no note, and the approval digest is left out. The engine sets that
/// digest itself (`set_visual_review`), so a caller's expected value does not have to carry it.
fn normalized_review(review: &VisualReview) -> VisualReview {
    let mut out = review.clone();
    out.note = out.note.filter(|note| !note.trim().is_empty());
    out.approved_digest = None;
    out
}

/// Where an element id lives. Panels, diagram nodes, and connectors share one id namespace.
#[derive(Clone, Copy)]
enum Locus {
    Panel(usize),
    Node { panel: usize, index: usize },
    Edge { panel: usize, index: usize },
}

fn locate(doc: &FigureDocument, id: &str) -> Option<Locus> {
    if let Some(i) = doc.panels.iter().position(|p| p.id == id) {
        return Some(Locus::Panel(i));
    }
    for (pi, p) in doc.panels.iter().enumerate() {
        if let PanelBody::Diagram(dg) = &p.body {
            if let Some(i) = dg.nodes.iter().position(|n| n.id == id) {
                return Some(Locus::Node { panel: pi, index: i });
            }
            if let Some(i) = dg.edges.iter().position(|e| e.id == id) {
                return Some(Locus::Edge { panel: pi, index: i });
            }
        }
    }
    None
}

/// Applies one operation to `doc`. `changed` receives the ids the operation touched.
pub fn apply(doc: &mut FigureDocument, op: &Operation, changed: &mut Vec<Id>) -> DltResult<()> {
    // Refused before anything is applied, so `patch.apply` never stores a v1.2 field (guard.rs).
    crate::guard::require_operations(doc, std::slice::from_ref(op))?;
    match op {
        Operation::PutDataset { dataset } => put_dataset(doc, dataset, changed),
        Operation::SetPlot { panel_id, plot } => {
            let current = plot_mut(doc, panel_id)?;
            if current.x_axis.label != plot.x_axis.label || current.y_axis.label != plot.y_axis.label {
                return Err(DltError::new(
                    ErrorCode::ContentLocked,
                    "axis captions are content: change them with set_axis_label, not set_plot",
                ));
            }
            // A data-only change must not clear or change the display mode silently.
            if current.display != plot.display {
                return Err(DltError::new(
                    ErrorCode::ContentLocked,
                    "the display mode is design: change it with set_plot_display, not set_plot",
                ));
            }
            *current = plot.clone();
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::SetPlotDisplay { panel_id, expected_display, display } => {
            let current = plot_mut(doc, panel_id)?;
            if current.display != *expected_display {
                return Err(DltError::new(
                    ErrorCode::RevisionConflict,
                    format!("plot \"{panel_id}\" shows display mode {:?}, not the expected {:?}", current.display, expected_display),
                ));
            }
            current.display = *display;
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::SetDiagramProvenance { panel_id, element_id, expected_origin, expected_evidence_ids, origin, evidence_ids } => {
            set_diagram_provenance(doc, panel_id, element_id, expected_origin.as_deref(), expected_evidence_ids, origin.as_deref(), evidence_ids, changed)
        }
        Operation::SetCommonLegend { expected, legend } => set_common_legend(doc, expected.as_ref(), legend.as_ref(), changed),
        Operation::SetFigureVisualReview { expected, review } => set_visual_review(doc, expected.as_ref(), review.as_ref(), changed),
        Operation::SetFigureImageTransform {
            panel_id,
            expected_asset_sha256,
            expected_crop_pixel_rect,
            expected_rotation_millidegrees,
            crop_pixel_rect,
            rotation_millidegrees,
        } => {
            let img = image_mut(doc, panel_id)?;
            check_image_asset(img, expected_asset_sha256)?;
            if img.crop_pixel_rect != *expected_crop_pixel_rect || img.rotation_millidegrees != *expected_rotation_millidegrees {
                return Err(stale(format!("the crop or rotation of panel \"{panel_id}\" changed since it was read")));
            }
            img.crop_pixel_rect = *crop_pixel_rect;
            img.rotation_millidegrees = *rotation_millidegrees;
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::SetFigureImageCalibration { panel_id, expected_asset_sha256, expected_calibration, calibration } => {
            let img = image_mut(doc, panel_id)?;
            check_image_asset(img, expected_asset_sha256)?;
            if img.calibration != *expected_calibration {
                return Err(stale(format!("the calibration of panel \"{panel_id}\" changed since it was read")));
            }
            img.calibration = calibration.clone();
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::SetAxisLabel { panel_id, axis, label } => {
            let pl = plot_mut(doc, panel_id)?;
            match axis.as_str() {
                "x" => pl.x_axis.label = label.clone(),
                "y" => pl.y_axis.label = label.clone(),
                other => return Err(invalid(format!("axis \"{other}\" must be x or y"))),
            }
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::SetAxisTicks { panel_id, axis, tick_step } => {
            let pl = plot_mut(doc, panel_id)?;
            match axis.as_str() {
                "x" => pl.x_axis.tick_step = tick_step.clone(),
                "y" => pl.y_axis.tick_step = tick_step.clone(),
                other => return Err(invalid(format!("axis \"{other}\" must be x or y"))),
            }
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::ArrangePanels { bounds } => {
            for (id, r) in bounds {
                let i = panel_index(doc, id)?;
                doc.panels[i].bounds = *r;
                changed.push(id.clone());
            }
            Ok(())
        }
        Operation::SetPanelLabel { panel_id, label } => {
            let i = panel_index(doc, panel_id)?;
            doc.panels[i].label = label.clone();
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::SetCaption { caption } => {
            doc.caption = caption.clone();
            changed.push("figure".to_owned());
            Ok(())
        }
        Operation::PutEvidence { evidence } => {
            put_evidence(doc, evidence);
            changed.push(evidence.id.clone());
            Ok(())
        }
        Operation::ConnectDiagram { panel_id, edge } => {
            let dg = diagram_mut(doc, panel_id)?;
            if dg.edges.iter().any(|e| e.id == edge.id) {
                return Err(DltError::new(ErrorCode::AlreadyExists, format!("connector \"{}\" exists", edge.id)));
            }
            dg.edges.push(edge.clone());
            changed.push(edge.id.clone());
            Ok(())
        }
        Operation::AddDiagramNode { panel_id, node, evidence_ids, origin } => {
            let dg = diagram_mut(doc, panel_id)?;
            if dg.nodes.iter().any(|n| n.id == node.id) {
                return Err(DltError::new(ErrorCode::AlreadyExists, format!("node \"{}\" exists", node.id)));
            }
            dg.nodes.push(node.clone());
            // An element without evidence has no entry, as SetDiagramProvenance leaves it. Both paths then hash the same way.
            if evidence_ids.is_empty() {
                dg.evidence_ids.remove(&node.id);
            } else {
                dg.evidence_ids.insert(node.id.clone(), evidence_ids.clone());
            }
            dg.node_origins.insert(node.id.clone(), origin.clone());
            changed.push(node.id.clone());
            Ok(())
        }
        Operation::AddPanel { index, panel } => add_panel(doc, *index, panel, changed),
        Operation::RemovePanel { panel_id } => {
            let i = panel_index(doc, panel_id)?;
            doc.panels.remove(i);
            changed.push(panel_id.clone());
            Ok(())
        }
        Operation::ReorderPanels { panel_ids } => reorder_panels(doc, panel_ids, changed),
        Operation::SetStyle { node_ids, patch } => {
            for id in node_ids {
                match locate(doc, id) {
                    Some(Locus::Panel(i)) => patch.apply(&mut doc.panels[i].style),
                    Some(Locus::Node { panel, index }) => match &mut doc.panels[panel].body {
                        PanelBody::Diagram(dg) => patch.apply(&mut dg.nodes[index].style),
                        _ => return Err(invalid("node index does not match its panel")),
                    },
                    Some(Locus::Edge { panel, index }) => match &mut doc.panels[panel].body {
                        PanelBody::Diagram(dg) => patch.apply(&mut dg.edges[index].style),
                        _ => return Err(invalid("connector index does not match its panel")),
                    },
                    None => return Err(not_found("element", id)),
                }
                changed.push(id.clone());
            }
            Ok(())
        }
        Operation::SetGeometry { node_id, bounds } => set_geometry(doc, node_id, *bounds, changed),
        Operation::RemoveNode { node_id } => remove_node(doc, node_id, changed),
        Operation::SetText { node_id, text } => match locate(doc, node_id) {
            Some(Locus::Node { panel, index }) => {
                if let PanelBody::Diagram(dg) = &mut doc.panels[panel].body {
                    dg.nodes[index].text = text.clone();
                }
                changed.push(node_id.clone());
                Ok(())
            }
            Some(Locus::Edge { panel, index }) => {
                if let PanelBody::Diagram(dg) = &mut doc.panels[panel].body {
                    dg.edges[index].label = text.clone();
                }
                changed.push(node_id.clone());
                Ok(())
            }
            _ => Err(not_found("node or connector", node_id)),
        },
        other => Err(DltError::unsupported(format!(
            "operation \"{}\" is not supported for figure artifacts",
            other.name()
        ))),
    }
}

fn add_panel(doc: &mut FigureDocument, index: u32, panel: &FigurePanel, changed: &mut Vec<Id>) -> DltResult<()> {
    if doc.panels.iter().any(|p| p.id == panel.id) {
        return Err(DltError::new(ErrorCode::AlreadyExists, format!("panel \"{}\" exists", panel.id)));
    }
    let at = index as usize;
    if at > doc.panels.len() {
        return Err(invalid(format!("panel index {index} is beyond the {} panels", doc.panels.len())));
    }
    doc.panels.insert(at, panel.clone());
    changed.push(panel.id.clone());
    Ok(())
}

fn reorder_panels(doc: &mut FigureDocument, panel_ids: &[Id], changed: &mut Vec<Id>) -> DltResult<()> {
    if panel_ids.len() != doc.panels.len() {
        return Err(invalid("reorder_panels must list every panel exactly once"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in panel_ids {
        panel_index(doc, id)?;
        if !seen.insert(id.clone()) {
            return Err(invalid(format!("panel \"{id}\" is listed twice")));
        }
    }
    let mut old: Vec<Option<FigurePanel>> = std::mem::take(&mut doc.panels).into_iter().map(Some).collect();
    let mut next = Vec::with_capacity(panel_ids.len());
    for id in panel_ids {
        let i = old
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| &p.id == id))
            .ok_or_else(|| not_found("panel", id))?;
        next.push(old[i].take().ok_or_else(|| not_found("panel", id))?);
    }
    doc.panels = next;
    changed.extend(panel_ids.iter().cloned());
    Ok(())
}

fn set_geometry(doc: &mut FigureDocument, id: &str, bounds: Rect, changed: &mut Vec<Id>) -> DltResult<()> {
    match locate(doc, id) {
        Some(Locus::Panel(i)) => doc.panels[i].bounds = bounds,
        Some(Locus::Node { panel, index }) => {
            if let PanelBody::Diagram(dg) = &mut doc.panels[panel].body {
                dg.nodes[index].bounds = bounds;
            }
        }
        _ => return Err(not_found("panel or node", id)),
    }
    changed.push(id.to_owned());
    Ok(())
}

fn remove_node(doc: &mut FigureDocument, node_id: &str, changed: &mut Vec<Id>) -> DltResult<()> {
    let Some(Locus::Node { panel, index }) = locate(doc, node_id) else {
        return Err(not_found("node", node_id));
    };
    let PanelBody::Diagram(dg) = &mut doc.panels[panel].body else {
        return Err(not_found("node", node_id));
    };
    if dg.edges.iter().any(|e| e.from == node_id || e.to == node_id) {
        return Err(invalid(format!("node \"{node_id}\" is an endpoint of a connector; remove the connector first")));
    }
    dg.nodes.remove(index);
    dg.evidence_ids.remove(node_id);
    dg.node_origins.remove(node_id);
    changed.push(node_id.to_owned());
    Ok(())
}

/// Sets the origin and evidence references of a diagram node or connector. The expected values must match the current ones.
/// A node keeps no evidence entry when the list is empty, as an unset node has none. A connector always has an origin.
#[allow(clippy::too_many_arguments)]
fn set_diagram_provenance(
    doc: &mut FigureDocument,
    panel_id: &str,
    element_id: &str,
    expected_origin: Option<&str>,
    expected_evidence: &[Id],
    origin: Option<&str>,
    evidence: &[Id],
    changed: &mut Vec<Id>,
) -> DltResult<()> {
    let dg = diagram_mut(doc, panel_id)?;
    if dg.nodes.iter().any(|n| n.id == element_id) {
        let current_origin = dg.node_origins.get(element_id).map(String::as_str);
        let current_evidence: &[Id] = dg.evidence_ids.get(element_id).map(Vec::as_slice).unwrap_or(&[]);
        if current_origin != expected_origin || current_evidence != expected_evidence {
            return Err(stale(format!("the provenance of \"{element_id}\" changed since it was read")));
        }
        match origin {
            Some(value) => dg.node_origins.insert(element_id.to_owned(), value.to_owned()),
            None => dg.node_origins.remove(element_id),
        };
        if evidence.is_empty() {
            dg.evidence_ids.remove(element_id);
        } else {
            dg.evidence_ids.insert(element_id.to_owned(), evidence.to_vec());
        }
    } else if let Some(edge) = dg.edges.iter_mut().find(|e| e.id == element_id) {
        if Some(edge.origin.as_str()) != expected_origin || edge.evidence_ids.as_slice() != expected_evidence {
            return Err(stale(format!("the provenance of \"{element_id}\" changed since it was read")));
        }
        let Some(value) = origin else {
            return Err(invalid(format!("connector \"{element_id}\" needs an origin")));
        };
        edge.origin = value.to_owned();
        edge.evidence_ids = evidence.to_vec();
    } else {
        return Err(not_found("node or connector", element_id));
    }
    changed.push(element_id.to_owned());
    Ok(())
}

/// Replaces or removes the common legend. The legend is checked as a whole, so a stored legend and a new one follow one rule.
fn set_common_legend(
    doc: &mut FigureDocument,
    expected: Option<&CommonLegend>,
    legend: Option<&CommonLegend>,
    changed: &mut Vec<Id>,
) -> DltResult<()> {
    if doc.common_legend.as_ref().map(normalized_legend) != expected.map(normalized_legend) {
        return Err(stale("the common legend changed since it was read"));
    }
    doc.common_legend = match legend {
        None => None,
        Some(next) => {
            let next = normalized_legend(next);
            let problems = validate::legend_problems(&next);
            if !problems.is_empty() {
                return Err(invalid(problems.join("; ")));
            }
            Some(next)
        }
    };
    changed.push(COMMON_LEGEND_ID.to_owned());
    Ok(())
}

/// Replaces or removes the visual review. The review is separate from every evidence `review_state`. An approval records the
/// digest of the figure as it is at this point of the change, so a later content, data, or design change in the same change makes it stale.
fn set_visual_review(doc: &mut FigureDocument, expected: Option<&VisualReview>, review: Option<&VisualReview>, changed: &mut Vec<Id>) -> DltResult<()> {
    if doc.visual_review.as_ref().map(normalized_review) != expected.map(normalized_review) {
        return Err(stale("the visual review changed since it was read"));
    }
    doc.visual_review = match review.map(normalized_review) {
        None => None,
        Some(mut next) => {
            // Only an approval carries a digest, and the engine writes it: a digest sent by the caller is replaced.
            next.approved_digest = match next.state {
                VisualReviewState::Approved => Some(digest::approval_digest(doc)?),
                VisualReviewState::Pending | VisualReviewState::NeedsRevision => None,
            };
            Some(next)
        }
    };
    changed.push(VISUAL_REVIEW_ID.to_owned());
    Ok(())
}

/// Records the page that a PDF evidence item was taken from. Citing text proves the page was read, so the
/// read set grows only by cited pages. Pages that were read but not cited are not recorded (conservative).
fn record_read_page(doc: &mut FigureDocument, evidence: &Evidence) {
    if evidence.origin != PDF_EVIDENCE_ORIGIN {
        return;
    }
    let Some(page) = evidence.physical_page_index else { return };
    match doc.source_scopes.iter_mut().find(|s| s.source_hash == evidence.source_hash) {
        Some(scope) => {
            if !scope.physical_page_indices.contains(&page) {
                scope.physical_page_indices.push(page);
                scope.physical_page_indices.sort_unstable();
            }
        }
        None => doc.source_scopes.push(SourceCoverage { source_hash: evidence.source_hash.clone(), physical_page_indices: vec![page] }),
    }
}

fn put_evidence(doc: &mut FigureDocument, evidence: &Evidence) {
    record_read_page(doc, evidence);
    match doc.evidence.iter().position(|e| e.id == evidence.id) {
        Some(i) => doc.evidence[i] = evidence.clone(),
        None => doc.evidence.push(evidence.clone()),
    }
}

/// Puts a dataset version. Replacing a dataset needs derived_from naming its current version, and the new version must keep the
/// transform history already recorded. The version hash is computed here.
///
/// The origin is computed, not claimed. A version of an existing id, or a derived copy, must carry the origin that its change gives
/// (`dataset::resulting_origin`): derived_transform when its cells, row order or columns with values differ from the version it
/// derives from, and that version's origin when they do not. The engine's change check refuses any other origin (InvalidArgument), and
/// a change of the data that no recorded transform extends (`dataset::change_findings`). The service gives the same origin to the same change.
///
/// A new id with no derived_from is a fresh import. Its origin is the importer's claim, because a genuinely new asset has no lineage
/// to check it against (see the limit in `dataset::reconcile_source`). A new id that carries a recorded history, or that reads a source
/// another dataset of this figure already holds, is a derivation in disguise, so it must name its parent and is checked against it.
///
/// The transforms that a version adds are applied with the service's own rules (`dataset::apply_edits`), so a PutDataset refuses
/// the transforms that `service::edit_dataset` refuses, with the same codes, a row selection included. The replay alone accepts some
/// of them, such as an ambiguous replacement or a repeated column name. A stored selection of an older file is still replayed.
fn put_dataset(doc: &mut FigureDocument, dataset_in: &Dataset, changed: &mut Vec<Id>) -> DltResult<()> {
    let mut ds = dataset_in.clone();
    if ds.id.trim().is_empty() {
        return Err(invalid("dataset id is empty"));
    }
    let existing = doc.datasets.iter().position(|d| d.id == ds.id);
    // The version that the transforms this version adds apply to. None for a fresh import.
    let base: Option<&Dataset> = match (&ds.derived_from, existing) {
        (None, Some(_)) => {
            return Err(DltError::new(
                ErrorCode::InvalidArgument,
                format!("dataset \"{}\" exists: a new version must name derived_from its current version", ds.id),
            ));
        }
        (Some(parent), Some(i)) => {
            let current = &doc.datasets[i];
            if parent.dataset_id != ds.id || parent.version_hash != current.version_hash {
                return Err(DltError::new(
                    ErrorCode::RevisionConflict,
                    format!("derived_from does not name the current version of \"{}\"", ds.id),
                ));
            }
            if current.original_asset_hash != ds.original_asset_hash {
                return Err(DltError::new(
                    ErrorCode::IntegrityError,
                    format!("dataset \"{}\" is read from a different source than its current version", ds.id),
                ));
            }
            if !dataset::history_extends(current, &ds) {
                return Err(DltError::new(
                    ErrorCode::IntegrityError,
                    format!("dataset \"{}\" rewrote its recorded transform history", ds.id),
                ));
            }
            Some(current)
        }
        (Some(parent), None) => match doc.datasets.iter().find(|d| d.id == parent.dataset_id) {
            Some(p) if p.version_hash == parent.version_hash => {
                // A new id derived from the parent is read from the parent's source and keeps the parent's recorded history.
                if p.original_asset_hash != ds.original_asset_hash {
                    return Err(DltError::new(
                        ErrorCode::IntegrityError,
                        format!("dataset \"{}\" is read from a different source than \"{}\"", ds.id, parent.dataset_id),
                    ));
                }
                if !dataset::history_extends(p, &ds) {
                    return Err(DltError::new(
                        ErrorCode::IntegrityError,
                        format!("dataset \"{}\" does not keep the recorded transform history of \"{}\"", ds.id, parent.dataset_id),
                    ));
                }
                Some(p)
            }
            Some(_) => {
                return Err(DltError::new(
                    ErrorCode::RevisionConflict,
                    format!("derived_from names an old version of \"{}\"", parent.dataset_id),
                ));
            }
            None => return Err(not_found("parent dataset", &parent.dataset_id)),
        },
        (None, None) => {
            if !ds.transforms.is_empty() || doc.datasets.iter().any(|d| d.original_asset_hash == ds.original_asset_hash) {
                let reason = if ds.transforms.is_empty() {
                    format!("dataset \"{}\" reads the source of another dataset in this figure", ds.id)
                } else {
                    format!("dataset \"{}\" carries a recorded history", ds.id)
                };
                let refused = [diag::blocking(
                    code::DATASET_LINEAGE_REQUIRED,
                    Some(ds.id.as_str()),
                    format!("{reason}, so it must name derived_from its parent version"),
                )];
                return Err(diag::to_error(&refused, "a new dataset id needs derived_from"));
            }
            None
        }
    };
    if let Some(base) = base {
        // The added transforms take the service's rules, so the refusal is the service's, with its code (`apply_edits`). The origin
        // and the recorded derivation are checked by the change check of the engine (`dataset::change_findings`), after the findings
        // that name the cause of a change (a column's type, a derived column's name, the cells).
        dataset::apply_edits(base, ds.transforms.get(base.transforms.len()..).unwrap_or(&[]))?;
    }
    let computed = dataset::compute_version(&ds).map_err(|e| DltError::new(ErrorCode::InvalidArgument, e.message))?;
    if !ds.version_hash.is_empty() && ds.version_hash != computed {
        return Err(DltError::new(
            ErrorCode::IntegrityError,
            format!("version_hash of \"{}\" does not match its canonical content", ds.id),
        ));
    }
    ds.version_hash = computed;
    let id = ds.id.clone();
    match existing {
        Some(i) => doc.datasets[i] = ds,
        None => doc.datasets.push(ds),
    }
    changed.push(id);
    Ok(())
}
