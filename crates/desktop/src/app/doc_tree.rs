use super::*;

pub(super) fn find_node<'a>(artifact: &'a Artifact, id: &str) -> Option<&'a Node> {
    match &artifact.body {
        ArtifactBody::Canvas(document) => find_node_in_pages(&document.pages, id),
        ArtifactBody::Slides(document) => find_node_in_pages(&document.pages, id),
        ArtifactBody::Flow(document) => document
            .sections
            .iter()
            .flat_map(|section| &section.blocks)
            .find_map(|block| find_node_in_flow(block, id)),
        ArtifactBody::Figure(document) => document.panels.iter().find_map(|panel| {
            if let designllm_contracts::PanelBody::Diagram(diagram) = &panel.body {
                diagram.nodes.iter().find_map(|node| find_node_in_node(node, id))
            } else {
                None
            }
        }),
        ArtifactBody::WebCode(_) => None,
    }
}

pub(super) fn find_node_in_pages<'a>(pages: &'a [Page], id: &str) -> Option<&'a Node> {
    pages
        .iter()
        .flat_map(|page| &page.nodes)
        .find_map(|node| find_node_in_node(node, id))
}

pub(super) fn inline_text_target_node_ids(artifact: &Artifact, kind: ArtifactKind, page_index: u32, node_id: &str) -> Option<Vec<String>> {
    if matches!(artifact.body, ArtifactBody::Flow(_)) {
        let paragraph = find_paragraph(artifact, node_id)?;
        return paragraph.runs.iter().all(|run| !run.protected).then(|| vec![node_id.to_owned()]);
    }
    let node = find_node(artifact, node_id)?;
    if node.kind == NodeKind::Text {
        return (node.ext.locked != Some(true)).then(|| vec![node.id.clone()]);
    }
    if kind != ArtifactKind::Canvas || node.kind != NodeKind::Rectangle {
        return None;
    }
    let ArtifactBody::Canvas(document) = &artifact.body else { return None };
    let page = document.pages.get(page_index as usize)?;
    let (table_node, cell, instance_root, nested_instance, locked_ancestor) =
        find_canvas_table_cell(&page.nodes, node_id, None, false, false)?;
    if nested_instance || locked_ancestor || cell.ext.locked == Some(true)
        || cell.kind != NodeKind::Rectangle || table_node.id == cell.id
    {
        return None;
    }
    let table = table_node.ext.table.as_ref()?;
    if canvas_table_cell_is_covered(table, node_id)? {
        return None;
    }
    if let Some(instance_root) = instance_root {
        if page.role.is_some() || instance_root.kind != NodeKind::Group {
            return None;
        }
        let reference = instance_root.ext.instance.as_ref()?;
        let source_page = document.pages.iter().find(|page| {
            page.id == reference.component_page_id && page.role == Some(designllm_contracts::PageRole::Component)
        })?;
        let prefix = format!("{}::", instance_root.id);
        let source_cell_id = cell.id.strip_prefix(&prefix)?;
        let source_table_id = table_node.id.strip_prefix(&prefix)?;
        let (source_table_node, source_cell, source_instance_root, source_nested, source_locked) =
            find_canvas_table_cell(&source_page.nodes, source_cell_id, None, false, false)?;
        if source_instance_root.is_some() || source_nested || source_locked || source_cell.ext.locked == Some(true)
            || source_table_node.id != source_table_id || source_cell.kind != NodeKind::Rectangle
        {
            return None;
        }
        let source_table = source_table_node.ext.table.as_ref()?;
        let materialized_table = table_node.ext.table.as_ref()?;
        if !component_table_mapping_matches(&instance_root.id, source_table, materialized_table, &table_node.children) {
            return None;
        }
        if canvas_table_cell_is_covered(source_table, source_cell_id)? {
            return None;
        }
        return Some(vec![cell.id.clone(), table_node.id.clone(), instance_root.id.clone()]);
    }
    Some(vec![cell.id.clone(), table_node.id.clone()])
}

pub(super) fn find_canvas_table_cell<'a>(
    nodes: &'a [Node],
    cell_id: &str,
    instance_root: Option<&'a Node>,
    nested_instance: bool,
    ancestor_locked: bool,
) -> Option<(&'a Node, &'a Node, Option<&'a Node>, bool, bool)> {
    for node in nodes {
        let (instance_root, nested_instance) = if node.ext.instance.is_some() {
            (Some(node), nested_instance || instance_root.is_some())
        } else {
            (instance_root, nested_instance)
        };
        let locked = ancestor_locked || node.ext.locked == Some(true);
        if node.ext.table.as_ref().is_some_and(|table| table.rows.iter().any(|row| row.cell_ids.iter().any(|id| id == cell_id))) {
            let cell = node.children.iter().find(|child| child.id == cell_id)?;
            return Some((node, cell, instance_root, nested_instance, locked));
        }
        if let Some(found) = find_canvas_table_cell(&node.children, cell_id, instance_root, nested_instance, locked) {
            return Some(found);
        }
    }
    None
}

pub(super) fn component_table_mapping_matches(
    instance_id: &str,
    source: &designllm_contracts::CanvasTable,
    materialized: &designllm_contracts::CanvasTable,
    materialized_children: &[Node],
) -> bool {
    let derived = |id: &str| format!("{instance_id}::{id}");
    if source.rows.len() != materialized.rows.len()
        || source.column_widths != materialized.column_widths
        || source.cell_spans.len() != materialized.cell_spans.len()
        || materialized_children.len() != source.rows.iter().map(|row| row.cell_ids.len()).sum::<usize>()
    {
        return false;
    }
    for (source_row, instance_row) in source.rows.iter().zip(&materialized.rows) {
        if instance_row.id != derived(&source_row.id) || source_row.cell_ids.len() != instance_row.cell_ids.len() {
            return false;
        }
        for (source_cell_id, instance_cell_id) in source_row.cell_ids.iter().zip(&instance_row.cell_ids) {
            if instance_cell_id != &derived(source_cell_id)
                || !materialized_children.iter().any(|child| child.id == *instance_cell_id && child.kind == NodeKind::Rectangle)
            {
                return false;
            }
        }
    }
    if source.header_row_id.as_deref().map(|id| derived(id))
        != materialized.header_row_id.as_deref().map(str::to_owned)
    {
        return false;
    }
    for (source_anchor, source_span) in &source.cell_spans {
        if materialized.cell_spans.get(&derived(source_anchor)) != Some(source_span) {
            return false;
        }
    }
    true
}

pub(super) fn canvas_image_node_with_state<'a>(
    nodes: &'a [Node],
    node_id: &str,
    inside_instance: bool,
    ancestor_locked: bool,
) -> Option<(&'a Node, bool, bool)> {
    for node in nodes {
        let nested = inside_instance || node.ext.instance.is_some();
        let locked = ancestor_locked || node.ext.locked == Some(true);
        if node.id == node_id {
            return Some((node, nested, locked));
        }
        if let Some(found) = canvas_image_node_with_state(&node.children, node_id, nested, locked) {
            return Some(found);
        }
    }
    None
}

/// Header-only BMP feedback for the picker. The Canvas operation validates the canonical imported bytes.
pub(super) fn bmp_dimensions(path: &Path) -> Option<(u32, u32)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header).ok()?;
    bmp_dimensions_from_header(&header)
}

pub(super) fn bmp_dimensions_from_header(header: &[u8]) -> Option<(u32, u32)> {
    if header.len() < 26 || &header[..2] != b"BM" {
        return None;
    }
    let dib_size = u32::from_le_bytes(header[14..18].try_into().ok()?);
    let (width, height) = if dib_size == 12 {
        (
            u32::from(u16::from_le_bytes(header[18..20].try_into().ok()?)),
            u32::from(u16::from_le_bytes(header[20..22].try_into().ok()?)),
        )
    } else if dib_size >= 40 {
        let width = i32::from_le_bytes(header[18..22].try_into().ok()?);
        let height = i32::from_le_bytes(header[22..26].try_into().ok()?);
        if width <= 0 { return None; }
        (width as u32, height.checked_abs()? as u32)
    } else {
        return None;
    };
    (width > 0 && height > 0).then_some((width, height))
}

pub(super) fn operation_target_ids(operations: &[Operation]) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for operation in operations {
        match operation {
            Operation::AddNode { node, .. } => { ids.insert(node.id.clone()); }
            Operation::RemoveNode { node_id }
            | Operation::SetText { node_id, .. }
            | Operation::SetGeometry { node_id, .. }
            | Operation::SetLayout { node_id, .. }
            | Operation::SetInteraction { node_id, .. }
            | Operation::SetInteractions { node_id, .. }
            | Operation::SetVariantStyle { node_id, .. }
            | Operation::SetZOrder { node_id, .. }
            | Operation::ReplaceImageAsset { node_id, .. }
            | Operation::SetNodeList { node_id, .. }
            | Operation::SetNodePath { node_id, .. } => { ids.insert(node_id.clone()); }
            Operation::ReplaceText { range, .. } => { ids.insert(range.paragraph_id.clone()); }
            Operation::SetStyle { node_ids, .. }
            | Operation::SetNodeAppearance { node_ids, .. }
            | Operation::SetNodeInfo { node_ids, .. } => { ids.extend(node_ids.iter().cloned()); }
            Operation::MoveNode { node_id, parent_id, .. } => {
                ids.insert(node_id.clone());
                ids.extend(parent_id.iter().cloned());
            }
            Operation::SetCanvasTableStructure { table_id, .. }
            | Operation::SetCanvasTableColumnWidths { table_id, .. } => { ids.insert(table_id.clone()); }
            Operation::InsertFlowBlock { section_id, .. } => { ids.insert(section_id.clone()); }
            Operation::UpdateFigureLink { link } => { ids.insert(link.target_node_id.clone()); }
            Operation::SetDiagramProvenance { element_id, .. } => { ids.insert(element_id.clone()); }
            Operation::SetFigureImageTransform { panel_id, .. } | Operation::SetFigureImageCalibration { panel_id, .. } => { ids.insert(panel_id.clone()); }
            // A repeat group and the data table it reads; the component page is not a node.
            Operation::SetGroupRepeat { node_id, expected, repeat } => {
                ids.insert(node_id.clone());
                ids.extend(repeat.iter().chain(expected.iter()).map(|spec| spec.table_id.clone()));
            }
            Operation::SetGroupCondition { node_id, .. } => { ids.insert(node_id.clone()); }
            // The common legend and the visual review belong to the figure as a whole.
            Operation::SetCommonLegend { .. } | Operation::SetFigureVisualReview { .. } => {}
            _ => {}
        }
    }
    ids.into_iter().collect()
}

pub(super) fn canvas_table_cell_is_covered(table: &designllm_contracts::CanvasTable, cell_id: &str) -> Option<bool> {
    let row_count = table.rows.len();
    let column_count = table.rows.first()?.cell_ids.len();
    if row_count == 0 || column_count == 0 || table.rows.iter().any(|row| row.cell_ids.len() != column_count) {
        return None;
    }
    let cell_count = row_count.checked_mul(column_count)?;
    let mut positions = BTreeMap::new();
    for (row_index, row) in table.rows.iter().enumerate() {
        for (column_index, id) in row.cell_ids.iter().enumerate() {
            let index = row_index.checked_mul(column_count)?.checked_add(column_index)?;
            if positions.insert(id.as_str(), index).is_some() {
                return None;
            }
        }
    }
    let target_index = *positions.get(cell_id)?;
    let mut occupied = vec![false; cell_count];
    let mut covered = false;
    for (anchor_id, span) in &table.cell_spans {
        let anchor_index = *positions.get(anchor_id.as_str())?;
        let anchor_row = anchor_index / column_count;
        let anchor_column = anchor_index % column_count;
        let row_span = usize::try_from(span.row_span).ok()?;
        let column_span = usize::try_from(span.column_span).ok()?;
        if row_span == 0 || column_span == 0 || (row_span == 1 && column_span == 1) {
            return None;
        }
        let row_end = anchor_row.checked_add(row_span)?;
        let column_end = anchor_column.checked_add(column_span)?;
        if row_end > row_count || column_end > column_count {
            return None;
        }
        for row in anchor_row..row_end {
            for column in anchor_column..column_end {
                let index = row.checked_mul(column_count)?.checked_add(column)?;
                if occupied[index] {
                    return None;
                }
                occupied[index] = true;
                if index == target_index && index != anchor_index {
                    covered = true;
                }
            }
        }
    }
    Some(covered)
}

pub(super) fn canvas_table_anchor_for_covered_cell(artifact: &Artifact, page_index: u32, cell_id: &str) -> Option<(String, bool)> {
    let ArtifactBody::Canvas(document) = &artifact.body else { return None };
    let page = document.pages.get(page_index as usize)?;
    let (table_node, cell, instance_root, nested_instance, _) = find_canvas_table_cell(&page.nodes, cell_id, None, false, false)?;
    if nested_instance || cell.kind != NodeKind::Rectangle { return None; }
    let table = table_node.ext.table.as_ref()?;
    let anchor_id = covered_table_anchor(table, cell_id)?;
    let Some(instance_root) = instance_root else {
        return Some((anchor_id, false));
    };
    if page.role.is_some() || instance_root.kind != NodeKind::Group { return None; }
    let reference = instance_root.ext.instance.as_ref()?;
    let prefix = format!("{}::", instance_root.id);
    let source_cell_id = cell.id.strip_prefix(&prefix)?;
    let source_table_id = table_node.id.strip_prefix(&prefix)?;
    let source_page = document.pages.iter().find(|page| {
        page.id == reference.component_page_id && page.role == Some(designllm_contracts::PageRole::Component)
    })?;
    let (source_table_node, source_cell, source_instance_root, source_nested, _) =
        find_canvas_table_cell(&source_page.nodes, source_cell_id, None, false, false)?;
    if source_instance_root.is_some() || source_nested || source_table_node.id.as_str() != source_table_id
        || source_cell.id.as_str() != source_cell_id || source_cell.kind != NodeKind::Rectangle
        || reference.overrides.contains_key(source_cell_id)
    {
        return None;
    }
    let source_table = source_table_node.ext.table.as_ref()?;
    if !component_table_mapping_matches(&instance_root.id, source_table, table, &table_node.children) { return None; }
    let source_anchor_id = anchor_id.strip_prefix(&prefix)?;
    if covered_table_anchor(source_table, source_cell_id).as_deref() != Some(source_anchor_id) { return None; }
    Some((anchor_id, true))
}

pub(super) fn covered_table_anchor(table: &designllm_contracts::CanvasTable, cell_id: &str) -> Option<String> {
    if !canvas_table_cell_is_covered(table, cell_id)? { return None; }
    let position = |id: &str| table.rows.iter().enumerate().find_map(|(row, table_row)| {
        table_row.cell_ids.iter().position(|candidate| candidate.as_str() == id).map(|column| (row, column))
    });
    let target = position(cell_id)?;
    for (anchor_id, span) in &table.cell_spans {
        let anchor = position(anchor_id)?;
        let row_end = anchor.0.checked_add(usize::try_from(span.row_span).ok()?)?;
        let column_end = anchor.1.checked_add(usize::try_from(span.column_span).ok()?)?;
        if target.0 >= anchor.0 && target.0 < row_end && target.1 >= anchor.1 && target.1 < column_end {
            return (target != anchor).then(|| anchor_id.clone());
        }
    }
    None
}

pub(super) fn find_node_in_node<'a>(node: &'a Node, id: &str) -> Option<&'a Node> {
    if node.id == id {
        return Some(node);
    }
    node.children
        .iter()
        .find_map(|child| find_node_in_node(child, id))
}

pub(super) fn find_node_in_flow<'a>(block: &'a FlowBlock, id: &str) -> Option<&'a Node> {
    match block {
        FlowBlock::Image(node) => find_node_in_node(node, id),
        FlowBlock::Table(table) => table.rows.iter().flatten().find_map(|cell| {
            cell.blocks
                .iter()
                .find_map(|child| find_node_in_flow(child, id))
        }),
        FlowBlock::Paragraph(_) | FlowBlock::Opaque { .. } => None,
    }
}

pub(super) fn find_paragraph<'a>(artifact: &'a Artifact, id: &str) -> Option<&'a Paragraph> {
    let ArtifactBody::Flow(document) = &artifact.body else {
        return None;
    };
    document
        .sections
        .iter()
        .flat_map(|section| &section.blocks)
        .find_map(|block| find_paragraph_in_flow(block, id))
}

pub(super) fn find_paragraph_in_flow<'a>(block: &'a FlowBlock, id: &str) -> Option<&'a Paragraph> {
    match block {
        FlowBlock::Paragraph(paragraph) if paragraph.id == id => Some(paragraph),
        FlowBlock::Table(table) => table.rows.iter().flatten().find_map(|cell| {
            cell.blocks
                .iter()
                .find_map(|child| find_paragraph_in_flow(child, id))
        }),
        _ => None,
    }
}

pub(super) fn paragraph_text(paragraph: &Paragraph) -> String {
    paragraph.runs.iter().map(|run| run.text.as_str()).collect()
}

pub(super) fn inline_target_text<'a>(artifact: &'a Artifact, id: &str) -> Option<std::borrow::Cow<'a, str>> {
    if let Some(paragraph) = find_paragraph(artifact, id) {
        return Some(std::borrow::Cow::Owned(paragraph_text(paragraph)));
    }
    find_node(artifact, id).map(|node| std::borrow::Cow::Borrowed(node.text.as_str()))
}

pub(super) fn slide_text_position(node_id: &str, paragraph_id: &str) -> Option<u32> {
    let prefix = format!("{node_id}#p");
    paragraph_id.strip_prefix(prefix.as_str())?.parse().ok()
}

pub(super) fn last_text_position(kind: ArtifactKind, node_id: &str, text: &str) -> (String, u32, u32) {
    if kind != ArtifactKind::Slides {
        return (node_id.to_owned(), 0, text.graphemes(true).count().min(u32::MAX as usize) as u32);
    }
    let paragraphs: Vec<&str> = text.split('\n').collect();
    let index = paragraphs.len().saturating_sub(1).min(u32::MAX as usize) as u32;
    let caret = paragraphs.last().map_or(0, |paragraph| paragraph.graphemes(true).count()).min(u32::MAX as usize) as u32;
    (format!("{node_id}#p{index}"), index, caret)
}

pub(super) fn global_caret(text: &str, paragraph_index: u32, local_caret: u32) -> u32 {
    let mut index = 0usize;
    for (paragraph, line) in text.split('\n').enumerate() {
        if paragraph == paragraph_index as usize {
            return index.saturating_add(local_caret as usize).min(u32::MAX as usize) as u32;
        }
        index = index.saturating_add(line.graphemes(true).count()).saturating_add(1);
    }
    text.graphemes(true).count().min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests {
    use designllm_contracts::{FlowBlock, Operation, PathSpec, Rect};

    use super::operation_target_ids;

    /// A flow block inserted into a section addresses that section: the id the core reserves for the same insertion.
    #[test]
    fn an_inserted_flow_block_addresses_its_section() {
        let insert = Operation::InsertFlowBlock { section_id: "s1".into(), index: 0, block: FlowBlock::Opaque { id: "o".into(), label: String::new() } };
        assert_eq!(operation_target_ids(&[insert]), ["s1"]);
    }

    /// A path edit addresses the node whose path it replaces: the id the core reserves for the same operation.
    #[test]
    fn a_path_edit_addresses_its_node() {
        let view_box = Rect { x: 0, y: 0, width: 24_000, height: 24_000 };
        let expected = PathSpec::from_svg("M 0 0 L 1 1", view_box).expect("valid path data");
        let path = PathSpec::from_svg("M 0 0 L 2 2", view_box).expect("valid path data");
        let edit = Operation::SetNodePath { node_id: "n1".into(), expected, path };
        assert_eq!(operation_target_ids(&[edit]), ["n1"]);
    }
}
