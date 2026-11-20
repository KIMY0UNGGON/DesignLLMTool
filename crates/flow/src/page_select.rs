//! Page selection of a laid-out flow document for export, shared by the DOCX, HWPX and HWP engines: the selected pages,
//! the link regions of those pages, and the diagnostics those pages still report.
use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{Diagnostic, DltError, DltResult, ErrorCode, FlowDocument, Id, Primitive, RenderPage, RenderScene};

use crate::tree::{collect_block_ids, find_node, FlowNode};

/// The pages a visual export writes: every page when `indices` is None, else the pages [`select_pages`] keeps. PNG and
/// SVG write one page, so INVALID_ARGUMENT unless exactly one page is left. `whole_document_codes`: see [`select_pages`].
pub fn select_export_pages(
    scene: &mut RenderScene,
    document: &FlowDocument,
    format: &str,
    indices: Option<&[u32]>,
    whole_document_codes: &[&str],
) -> DltResult<()> {
    if let Some(list) = indices {
        select_pages(scene, document, list, whole_document_codes)?;
    }
    if matches!(format, "png" | "svg" | "svg-outline" | "svg_outline" | "svg-outlines") && scene.pages.len() != 1 {
        return Err(DltError::invalid(format!(
            "{format} export writes one page; {} selected (pass page_indices with one entry)",
            scene.pages.len()
        )));
    }
    Ok(())
}

/// Keeps the pages of `scene` that `indices` names, in the requested order (a repeated index once). INVALID_ARGUMENT for
/// an empty list, NOT_FOUND for an index that is not a page of the scene. The link regions of other pages are dropped,
/// and the diagnostics follow the page-range rule of `PageScope`. A diagnostic whose code is in `whole_document_codes`
/// is always kept, whatever node it names: it reports content that belongs on every page (headers and footers) or content
/// the scene does not draw and whose page is not known (a floating object named by its anchor).
pub fn select_pages(scene: &mut RenderScene, document: &FlowDocument, indices: &[u32], whole_document_codes: &[&str]) -> DltResult<()> {
    if indices.is_empty() {
        return Err(DltError::invalid("page_indices must not be empty; omit it to export every page"));
    }
    let mut order: Vec<usize> = Vec::with_capacity(indices.len());
    for &index in indices {
        let Some(position) = scene.pages.iter().position(|p| p.page_index == index) else {
            return Err(DltError::new(ErrorCode::NotFound, format!("page {index} does not exist ({} pages)", scene.pages.len())));
        };
        if !order.contains(&position) {
            order.push(position);
        }
    }
    let mut diagnostics = std::mem::take(&mut scene.diagnostics);
    {
        let scope = PageScope::new(document, &scene.pages, indices);
        diagnostics.retain(|d| whole_document_codes.contains(&d.code.as_str()) || scope.keeps(d));
    }
    scene.diagnostics = diagnostics;
    let mut pages: Vec<Option<RenderPage>> = std::mem::take(&mut scene.pages).into_iter().map(Some).collect();
    scene.pages = order.into_iter().filter_map(|position| pages[position].take()).collect();
    // A link region names its page, so the regions of pages that are not selected stay out.
    let selected = &scene.pages;
    scene.links.retain(|link| selected.iter().any(|p| p.id == link.page_id));
    Ok(())
}

/// UNSUPPORTED_OPERATION for a page selection on an export that writes the whole document (a native format).
pub fn refuse_page_selection(format: &str, indices: Option<&[u32]>) -> DltResult<()> {
    match indices {
        None => Ok(()),
        Some(_) => Err(DltError::unsupported(format!(
            "{format} export writes the whole document; page_indices is not supported for {format} (omit it, or export pages as pdf, png, or svg)"
        ))),
    }
}

/// The page-range rule for diagnostics. A diagnostic of a node that is drawn (directly, or by the runs, paragraphs or
/// cells inside it) belongs to the pages that draw it, and a page selection keeps it only when one of those pages is
/// selected. A diagnostic without a node, or of a node no page draws (content that is not drawn), belongs to the whole
/// document and is always kept.
struct PageScope<'a> {
    document: &'a FlowDocument,
    /// Node id -> the page indexes whose primitives or source regions name it.
    drawn: BTreeMap<&'a str, BTreeSet<u32>>,
    selected: &'a [u32],
}

impl<'a> PageScope<'a> {
    /// `pages` are all the pages of the laid-out document, before the selection.
    fn new(document: &'a FlowDocument, pages: &'a [RenderPage], selected: &'a [u32]) -> Self {
        let mut drawn: BTreeMap<&str, BTreeSet<u32>> = BTreeMap::new();
        for page in pages {
            let primitives = page.primitives.iter().map(|primitive| match primitive {
                Primitive::Text { node_id, .. }
                | Primitive::Shape { node_id, .. }
                | Primitive::Line { node_id, .. }
                | Primitive::Image { node_id, .. }
                | Primitive::Path { node_id, .. } => node_id.as_str(),
            });
            let regions = page.source_map.iter().flat_map(|region| {
                std::iter::once(region.node_id.as_str()).chain(region.logical_range.as_ref().map(|r| r.paragraph_id.as_str()))
            });
            for id in primitives.chain(regions) {
                drawn.entry(id).or_default().insert(page.page_index);
            }
        }
        PageScope { document, drawn, selected }
    }

    fn pages_of(&self, ids: &[&str]) -> BTreeSet<u32> {
        ids.iter().filter_map(|id| self.drawn.get(id)).flatten().copied().collect()
    }

    /// Whether the selection still reports `diagnostic`.
    fn keeps(&self, diagnostic: &Diagnostic) -> bool {
        let Some(id) = diagnostic.node_id.as_deref() else { return true };
        let node = find_node(self.document, id);
        let mut inner: Vec<Id> = Vec::new();
        match node {
            Some(FlowNode::Section(section)) => collect_block_ids(&section.blocks, &mut inner),
            Some(FlowNode::Paragraph(p)) => inner.extend(p.runs.iter().map(|r| r.id.clone())),
            Some(FlowNode::Table(table)) => {
                for cell in table.rows.iter().flatten() {
                    inner.push(cell.id.clone());
                    collect_block_ids(&cell.blocks, &mut inner);
                }
            }
            Some(FlowNode::Cell { cell, .. }) => collect_block_ids(&cell.blocks, &mut inner),
            Some(FlowNode::Image(image)) => inner.extend(image.children.iter().map(|c| c.id.clone())),
            Some(FlowNode::Run { .. } | FlowNode::Opaque { .. }) | None => {}
        }
        let mut ids: Vec<&str> = inner.iter().map(String::as_str).collect();
        ids.push(id);
        let mut pages = self.pages_of(&ids);
        // A run no primitive names (an empty or hidden run) is placed by its paragraph.
        if let (true, Some(FlowNode::Run { paragraph, .. })) = (pages.is_empty(), node) {
            pages = self.pages_of(&[paragraph.id.as_str()]);
        }
        pages.is_empty() || self.selected.iter().any(|index| pages.contains(index))
    }
}
