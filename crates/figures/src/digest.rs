//! Content and data digests. The core checks them against Operation::intent(): a design-only change must
//! leave both unchanged, and a content or data change needs the matching intent.
//!
//! Content: captions, panel order and labels, diagram nodes and connectors with their text and provenance (and the math
//! syntax of a math node, whose text is its source),
//! image assets and origins, evidence records, source scopes, and the common legend when the figure has one.
//! Data: dataset versions (canonical, without the version field itself), plot bindings, data columns,
//! axis unit, scale and explicit range, error definitions, and image calibrations.
//! Design (excluded from both, but in the approval digest through `design_fingerprint`): bounds, styles, tick steps,
//! arrangement, crop and rotation, the display mode of a plot, the node kind, and the canvas size.
//! Excluded from both: the visual review. Approving a figure changes neither what it shows nor its data, so a parent
//! document that links the figure does not see a content change.

use designllm_contracts::{ContentDigest, DltError, DltResult, FigureDocument, PanelBody, json_hash};
use serde_json::{Value, json};

use crate::dataset::compute_version;

/// The digest a visual approval is given for (R07.8): the content digest, the data digest, and the design fingerprint. A content,
/// data, or design change makes an approval stale. The content and data digests themselves are unchanged (P0).
pub fn approval_digest(doc: &FigureDocument) -> DltResult<String> {
    let base = digests(doc)?;
    json_hash(&json!({"content": base.content, "data": base.data, "design": design_fingerprint(doc)}))
}

/// The design that the content and data digests leave out, and the node kinds (`Node.kind` is in neither digest). The design
/// operations `SetStyle`, `SetGeometry`, `ArrangePanels`, `SetAxisTicks`, `SetPlotDisplay`, and `SetFigureImageTransform` change
/// these fields, and so does a node replaced by one of another kind. Nodes and connectors are listed in drawing order. Node children,
/// assets, layouts, interactions, and variants are stored but never rendered, and no change may add them (guard.rs), so they are left out.
fn design_fingerprint(doc: &FigureDocument) -> Value {
    let panels: Vec<Value> = doc
        .panels
        .iter()
        .map(|p| {
            let body = match &p.body {
                PanelBody::Diagram(dg) => json!({
                    "nodes": dg.nodes.iter().map(|n| json!({"id": n.id, "kind": n.kind, "bounds": n.bounds, "style": n.style})).collect::<Vec<_>>(),
                    "edges": dg.edges.iter().map(|e| json!({"id": e.id, "style": e.style})).collect::<Vec<_>>(),
                }),
                PanelBody::Plot(pl) => json!({"x_tick_step": pl.x_axis.tick_step, "y_tick_step": pl.y_axis.tick_step, "display": pl.display}),
                PanelBody::Image(img) => json!({"crop_pixel_rect": img.crop_pixel_rect, "rotation_millidegrees": img.rotation_millidegrees}),
            };
            json!({"id": p.id, "bounds": p.bounds, "style": p.style, "body": body})
        })
        .collect();
    json!({"size": doc.size, "panels": panels})
}

pub fn digests(doc: &FigureDocument) -> DltResult<ContentDigest> {
    let mut evidence: Vec<&designllm_contracts::Evidence> = doc.evidence.iter().collect();
    evidence.sort_by(|a, b| a.id.cmp(&b.id));
    let mut panels: Vec<Value> = Vec::with_capacity(doc.panels.len());
    for p in &doc.panels {
        let body = match &p.body {
            PanelBody::Diagram(dg) => {
                let mut nodes: Vec<&designllm_contracts::Node> = dg.nodes.iter().collect();
                nodes.sort_by(|a, b| a.id.cmp(&b.id));
                let mut edges: Vec<&designllm_contracts::Edge> = dg.edges.iter().collect();
                edges.sort_by(|a, b| a.id.cmp(&b.id));
                json!({
                    "nodes": nodes.iter().map(|n| {
                        let mut node = json!({
                            "id": n.id,
                            "text": n.text,
                            "origin": dg.node_origins.get(&n.id),
                            "evidence_ids": dg.evidence_ids.get(&n.id),
                        });
                        // Only a math node has the key, so a node without math keeps its digest (P0).
                        if let Some(syntax) = n.ext.math {
                            node["math"] = json!(syntax);
                        }
                        node
                    }).collect::<Vec<_>>(),
                    "edges": edges.iter().map(|e| json!({
                        "id": e.id,
                        "from": e.from,
                        "to": e.to,
                        "label": e.label,
                        "evidence_ids": e.evidence_ids,
                        "origin": e.origin,
                    })).collect::<Vec<_>>(),
                })
            }
            PanelBody::Plot(pl) => json!({"x_label": pl.x_axis.label, "y_label": pl.y_axis.label}),
            PanelBody::Image(img) => json!({
                "asset_sha256": img.asset.sha256,
                "origin": img.origin,
                "source_ref": img.source_ref,
                "pixel_width": img.pixel_width,
                "pixel_height": img.pixel_height,
            }),
        };
        let kind = match &p.body {
            PanelBody::Diagram(_) => "diagram",
            PanelBody::Plot(_) => "plot",
            PanelBody::Image(_) => "image",
        };
        panels.push(json!({"id": p.id, "label": p.label, "kind": kind, "body": body}));
    }
    let mut content = json!({
        "caption": doc.caption,
        "evidence": evidence,
        "source_scopes": doc.source_scopes,
        "panels": panels,
    });
    // Only a figure with a legend has the key, so a figure without one keeps its digest.
    if let Some(legend) = &doc.common_legend {
        content["common_legend"] = serde_json::to_value(legend).map_err(|e| DltError::invalid(e.to_string()))?;
    }

    let mut datasets = Vec::with_capacity(doc.datasets.len());
    for ds in &doc.datasets {
        datasets.push(json!({"id": ds.id, "version": compute_version(ds)?}));
    }
    let mut plots = Vec::new();
    let mut calibrations = Vec::new();
    for p in &doc.panels {
        match &p.body {
            PanelBody::Plot(pl) => plots.push(json!({
                "panel": p.id,
                "kind": pl.kind,
                "dataset_ref": pl.dataset_ref,
                "x_column": pl.x_column,
                "y_columns": pl.y_columns,
                "x_axis": {"scale": pl.x_axis.scale, "unit": pl.x_axis.unit, "minimum": pl.x_axis.minimum, "maximum": pl.x_axis.maximum},
                "y_axis": {"scale": pl.y_axis.scale, "unit": pl.y_axis.unit, "minimum": pl.y_axis.minimum, "maximum": pl.y_axis.maximum},
                "error": pl.error,
            })),
            PanelBody::Image(img) => {
                if let Some(cal) = &img.calibration {
                    calibrations.push(json!({"panel": p.id, "calibration": cal}));
                }
            }
            PanelBody::Diagram(_) => {}
        }
    }
    let data = json!({"datasets": datasets, "plots": plots, "calibrations": calibrations});

    Ok(ContentDigest { content: json_hash(&content)?, data: json_hash(&data)? })
}
