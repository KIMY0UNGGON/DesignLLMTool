//! Evidence records. PDF evidence (origin `paper_evidence`, the sources engine's origin) names a page, a region,
//! and the page geometry; table-cell evidence names a dataset cell; user statements carry their own text. A PDF
//! page can be cited only if the figure records that the page was read (source_scopes). Nothing is cited from a
//! page that was not read.

use std::collections::BTreeSet;

use designllm_contracts::figure_rules::{self, REVIEW_STATE_UNREVIEWED};
use designllm_contracts::{AssetStore, Diagnostic, Evidence, FigureDocument, PanelBody, PdfPageGeometry, Rect, sha256};

use crate::decimal::Dec;
use crate::diag::{self, code};

pub const REVIEW_STATES: [&str; 3] = figure_rules::REVIEW_STATES;
/// Origin of text cited from a PDF page. The only spelling accepted for PDF region evidence.
pub const PDF_EVIDENCE_ORIGIN: &str = figure_rules::PAPER_EVIDENCE_ORIGIN;

/// Converts a region measured from the crop box top-left (x right, y down) to PDF user space
/// (x right, y up). Returns (left, bottom, right, top), following docs/API_V1.md.
pub fn region_in_pdf_space(geo: &PdfPageGeometry, region: Rect) -> (i64, i64, i64, i64) {
    let left = geo.crop_box.x_min + region.x;
    let top = geo.crop_box.y_max - region.y;
    let bottom = top - region.height;
    (left, bottom, left + region.width, top)
}

fn geometry_issues(geo: &PdfPageGeometry, node: &str, d: &mut Vec<Diagnostic>) {
    let bad = |msg: &str, d: &mut Vec<Diagnostic>| d.push(diag::blocking(code::EVIDENCE_GEOMETRY_INVALID, Some(node), msg.to_owned()));
    match Dec::parse(&geo.user_unit) {
        Ok(u) if u.coef > 0 && !u.neg => {}
        _ => bad("user_unit must be a positive decimal", d),
    }
    if ![0u16, 90, 180, 270].contains(&geo.rotation_degrees) {
        bad("rotation must be 0, 90, 180 or 270 degrees", d);
    }
    for (name, b) in [("media_box", &geo.media_box), ("crop_box", &geo.crop_box)] {
        if b.x_min >= b.x_max || b.y_min >= b.y_max {
            bad(&format!("{name} is empty or inverted"), d);
        }
    }
    let (m, c) = (&geo.media_box, &geo.crop_box);
    if c.x_min < m.x_min || c.y_min < m.y_min || c.x_max > m.x_max || c.y_max > m.y_max {
        bad("crop_box is outside media_box", d);
    }
}

/// Validates one evidence record against the figure and the asset store.
pub fn check(ev: &Evidence, doc: &FigureDocument, assets: &dyn AssetStore, d: &mut Vec<Diagnostic>) {
    let node = Some(ev.id.as_str());
    if !REVIEW_STATES.contains(&ev.review_state.as_str()) {
        d.push(diag::blocking(
            code::EVIDENCE_REVIEW_STATE_INVALID,
            node,
            format!("review_state \"{}\" is not one of {REVIEW_STATES:?}", ev.review_state),
        ));
    }
    if ev.origin == "user_statement" {
        if ev.text.trim().is_empty() {
            d.push(diag::blocking(code::EVIDENCE_TEXT_EMPTY, node, "user statement evidence has no text"));
        } else if ev.source_hash != sha256(ev.text.as_bytes()) {
            d.push(diag::blocking(code::EVIDENCE_HASH_MISMATCH, node, "source_hash is not the hash of the statement text"));
        }
    } else if ev.origin == PDF_EVIDENCE_ORIGIN {
        let (Some(page), Some(region), Some(geo)) = (ev.physical_page_index, ev.region, ev.pdf_geometry.as_ref()) else {
            d.push(diag::blocking(
                code::EVIDENCE_GEOMETRY_INVALID,
                node,
                "paper_evidence needs physical_page_index, region and pdf_geometry",
            ));
            return;
        };
        if ev.text.trim().is_empty() {
            d.push(diag::blocking(code::EVIDENCE_TEXT_EMPTY, node, "paper_evidence has no extracted text"));
        }
        geometry_issues(geo, ev.id.as_str(), d);
        let (cw, ch) = (geo.crop_box.x_max - geo.crop_box.x_min, geo.crop_box.y_max - geo.crop_box.y_min);
        let inside = region.width > 0
            && region.height > 0
            && region.x >= 0
            && region.y >= 0
            && region.x.checked_add(region.width).is_some_and(|r| r <= cw)
            && region.y.checked_add(region.height).is_some_and(|b| b <= ch);
        if !inside {
            d.push(diag::blocking(code::EVIDENCE_REGION_OUTSIDE_CROP, node, "region is not inside the crop box"));
        }
        if assets.lookup(&ev.source_hash).is_err() {
            d.push(diag::blocking(code::SOURCE_ASSET_MISSING, node, "PDF source asset is not in the asset store"));
        }
        let read = doc
            .source_scopes
            .iter()
            .any(|s| s.source_hash == ev.source_hash && s.physical_page_indices.contains(&page));
        if !read {
            d.push(diag::blocking(
                code::EVIDENCE_PAGE_NOT_IN_SCOPE,
                node,
                format!("physical page {page} is not recorded as read for this source; it cannot be cited"),
            ));
        }
    } else if let Some(rest) = ev.origin.strip_prefix("table_cell:") {
        let parts: Vec<&str> = rest.split(':').collect();
        let cell = match parts.as_slice() {
            [ds_id, row_id, col_id] => doc.datasets.iter().find(|x| x.id == *ds_id).and_then(|ds| {
                ds.rows.iter().find(|r| r.id == *row_id).and_then(|r| r.cells.get(*col_id)).map(|c| (ds, c))
            }),
            _ => None,
        };
        match cell {
            None => d.push(diag::blocking(
                code::EVIDENCE_TABLE_REF_INVALID,
                node,
                format!("table_cell origin \"{}\" does not name an existing cell", ev.origin),
            )),
            Some((ds, c)) => {
                if ev.source_hash != ds.original_asset_hash {
                    d.push(diag::blocking(code::EVIDENCE_HASH_MISMATCH, node, "table cell evidence must reference its dataset source"));
                }
                if ev.text != c.original {
                    d.push(diag::blocking(
                        code::EVIDENCE_TABLE_REF_INVALID,
                        node,
                        "evidence text must equal the original cell text",
                    ));
                }
            }
        }
    } else {
        d.push(diag::blocking(
            code::EVIDENCE_KIND_INVALID,
            node,
            format!("origin \"{}\" is not paper_evidence, user_statement, or table_cell:<dataset>:<row>:<column>", ev.origin),
        ));
    }
}

/// A cited PDF record is informational during editing and a loss requiring consent during export.
pub const UNREVIEWED_CITED: &str = "EVIDENCE_UNREVIEWED_CITED";

/// Reports each figure element that cites a PDF evidence record whose review_state is still `unreviewed`. A
/// citation is an element's evidence list, a connector's evidence list, an image's source_ref, or an image's
/// calibration evidence. Only the report is added: evidence, citations, and records that nothing cites stay as they are.
pub fn report_unreviewed_citations(doc: &FigureDocument, d: &mut Vec<Diagnostic>) {
    let pending: BTreeSet<&str> = doc
        .evidence
        .iter()
        .filter(|e| e.origin == PDF_EVIDENCE_ORIGIN && e.review_state == REVIEW_STATE_UNREVIEWED)
        .map(|e| e.id.as_str())
        .collect();
    if pending.is_empty() {
        return;
    }
    let mut cited: BTreeSet<(&str, &str)> = BTreeSet::new();
    for p in &doc.panels {
        let pid: &str = p.id.as_str();
        match &p.body {
            PanelBody::Diagram(dg) => {
                for (element, refs) in &dg.evidence_ids {
                    let element: &str = element.as_str();
                    cited.extend(refs.iter().map(|r| (element, r.as_str())));
                }
                for e in &dg.edges {
                    let eid: &str = e.id.as_str();
                    cited.extend(e.evidence_ids.iter().map(|r| (eid, r.as_str())));
                }
            }
            PanelBody::Image(img) => {
                cited.extend(img.source_ref.iter().map(|r| (pid, r.as_str())));
                cited.extend(img.calibration.iter().filter_map(|c| c.evidence_id.as_deref()).map(|r| (pid, r)));
            }
            PanelBody::Plot(_) => {}
        }
    }
    for (element, evidence_id) in cited {
        if pending.contains(evidence_id) {
            d.push(diag::info(
                UNREVIEWED_CITED,
                Some(element),
                format!("\"{element}\" cites \"{evidence_id}\", PDF evidence the user has not reviewed"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::PdfBox;

    #[test]
    fn pdf_region_maps_to_user_space_as_the_api_states() {
        let geo = PdfPageGeometry {
            media_box: PdfBox { x_min: 0, y_min: 0, x_max: 7_772_400, y_max: 10_058_400 },
            crop_box: PdfBox { x_min: 228_600, y_min: 254_000, x_max: 7_543_800, y_max: 9_652_000 },
            user_unit: "1".into(),
            rotation_degrees: 0,
            printed_page_label: Some("9".into()),
        };
        let r = Rect { x: 914_400, y: 762_000, width: 3_302_000, height: 254_000 };
        assert_eq!(region_in_pdf_space(&geo, r), (1_143_000, 8_636_000, 4_445_000, 8_890_000));
    }
}
