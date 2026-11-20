//! The stricter OpenUrl rule for new edits (backlog flow 26). [`crate::check_interaction_url`] runs whenever a value is
//! read, stored documents included, so it stays as it was and files written before this rule still open. A URL that an
//! operation newly sets must also have no Unicode format character (general category Cf, such as U+200B, U+202E, and
//! U+FEFF): such a character is invisible or reorders the text a reader sees.
use crate::{check_interaction_url, DltError, DltResult, FlowBlock, Interaction, Node, Operation, PanelBody};

/// Code point ranges of general category Cf in Unicode 15.1.
const FORMAT_CHARACTERS: &[(char, char)] = &[
    ('\u{00AD}', '\u{00AD}'), ('\u{0600}', '\u{0605}'), ('\u{061C}', '\u{061C}'), ('\u{06DD}', '\u{06DD}'),
    ('\u{070F}', '\u{070F}'), ('\u{0890}', '\u{0891}'), ('\u{08E2}', '\u{08E2}'), ('\u{180E}', '\u{180E}'),
    ('\u{200B}', '\u{200F}'), ('\u{202A}', '\u{202E}'), ('\u{2060}', '\u{2064}'), ('\u{2066}', '\u{206F}'),
    ('\u{FEFF}', '\u{FEFF}'), ('\u{FFF9}', '\u{FFFB}'), ('\u{110BD}', '\u{110BD}'), ('\u{110CD}', '\u{110CD}'),
    ('\u{13430}', '\u{1343F}'), ('\u{1BCA0}', '\u{1BCA3}'), ('\u{1D173}', '\u{1D17A}'), ('\u{E0001}', '\u{E0001}'),
    ('\u{E0020}', '\u{E007F}'),
];

fn is_format_character(c: char) -> bool {
    FORMAT_CHARACTERS.iter().any(|(first, last)| (*first..=*last).contains(&c))
}

/// [`check_interaction_url`], and no Unicode format character. INVALID_ARGUMENT otherwise.
pub fn check_new_interaction_url(url: &str) -> DltResult<()> {
    check_interaction_url(url)?;
    match url.chars().find(|c| is_format_character(*c)) {
        Some(c) => Err(DltError::invalid(format!("interaction url {url:?} must not contain the format character U+{:04X}", u32::from(c)))),
        None => Ok(()),
    }
}

fn check_interactions(interactions: &[Interaction]) -> DltResult<()> {
    interactions.iter().try_for_each(|interaction| match interaction {
        Interaction::OpenUrl { url } => check_new_interaction_url(url),
        _ => Ok(()),
    })
}

impl Operation {
    /// Checks every OpenUrl this operation sets with [`check_new_interaction_url`]: those of `SetInteraction` and
    /// `SetInteractions`, and those of the nodes (children included) that `AddNode`, `AddDiagramNode`, `InsertPage`,
    /// `AddPanel` (a diagram panel's nodes), and `InsertFlowBlock` (an image block, and image blocks in table cells at
    /// any depth) add. These are all the operations that carry a `Node`.
    pub fn check_new_interaction_urls(&self) -> DltResult<()> {
        let mut nodes: Vec<&Node> = match self {
            Self::SetInteraction { interaction, .. } => return check_interactions(std::slice::from_ref(interaction)),
            Self::SetInteractions { interactions, .. } => return check_interactions(interactions),
            Self::AddNode { node, .. } | Self::AddDiagramNode { node, .. } => vec![node],
            Self::InsertPage { page, .. } => page.nodes.iter().collect(),
            Self::AddPanel { panel, .. } => match &panel.body {
                PanelBody::Diagram(diagram) => diagram.nodes.iter().collect(),
                PanelBody::Plot(_) | PanelBody::Image(_) => return Ok(()),
            },
            Self::InsertFlowBlock { block, .. } => {
                let mut nodes = Vec::new();
                let mut blocks = vec![block];
                while let Some(block) = blocks.pop() {
                    match block {
                        FlowBlock::Image(node) => nodes.push(node),
                        FlowBlock::Table(table) => blocks.extend(table.rows.iter().flatten().flat_map(|cell| &cell.blocks)),
                        FlowBlock::Paragraph(_) | FlowBlock::Opaque { .. } => {}
                    }
                }
                nodes
            }
            _ => return Ok(()),
        };
        while let Some(node) = nodes.pop() {
            check_interactions(&node.interactions)?;
            nodes.extend(&node.children);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CanvasDocument, Diagram, FigurePanel, FlowBlock, NodeExt, NodeKind, Page, PanelBody, Rect, Size, Style, Table, TableCell};
    use std::collections::BTreeMap;

    /// A one-cell flow table holding `blocks`.
    fn table(blocks: Vec<FlowBlock>) -> FlowBlock {
        FlowBlock::Table(Table { id: "t".into(), column_widths: vec![1], rows: vec![vec![TableCell { id: "c".into(), blocks, column_span: 1, row_span: 1 }]], style: Style::default() })
    }

    const FORMAT_URLS: [&str; 3] = ["https://example.com/\u{200B}a", "https://example.com/\u{202E}gpj.exe", "https://\u{FEFF}example.com/"];

    fn node(id: &str, url: &str, children: Vec<Node>) -> Node {
        Node {
            id: id.into(), kind: NodeKind::Text, bounds: Rect::default(), text: String::new(), style: Style::default(), asset: None,
            children, layout: None, interactions: vec![Interaction::OpenUrl { url: url.into() }], variants: BTreeMap::new(), ext: NodeExt::default(),
        }
    }

    #[test]
    fn a_stored_document_with_a_format_character_url_still_reads() {
        for url in FORMAT_URLS {
            let mut page = Page::new("p", Size::slide());
            page.nodes.push(node("n", url, vec![]));
            let stored = serde_json::to_value(CanvasDocument { pages: vec![page], ..Default::default() }).unwrap();
            let read: CanvasDocument = serde_json::from_value(stored).expect("the read rule is unchanged");
            assert!(matches!(&read.pages[0].nodes[0].interactions[0], Interaction::OpenUrl { url: read } if read == url));
            check_interaction_url(url).expect("the read rule lets format characters through");
        }
    }

    #[test]
    fn a_new_operation_refuses_a_format_character_url() {
        for url in FORMAT_URLS {
            let operations = [
                Operation::SetInteraction { node_id: "n".into(), interaction: Interaction::OpenUrl { url: url.into() } },
                Operation::SetInteractions { node_id: "n".into(), interactions: vec![Interaction::Back, Interaction::OpenUrl { url: url.into() }] },
                Operation::AddNode { page_id: "p".into(), parent_id: None, node: node("parent", "https://example.com/", vec![node("child", url, vec![])]) },
                Operation::AddDiagramNode { panel_id: "f".into(), node: node("d", url, vec![]), evidence_ids: vec![], origin: "user".into() },
                Operation::InsertPage { index: 0, page: Page { nodes: vec![node("n", url, vec![])], ..Page::new("p", Size::slide()) } },
                Operation::InsertFlowBlock { section_id: "s".into(), index: 0, block: FlowBlock::Image(node("image", url, vec![])) },
                Operation::InsertFlowBlock { section_id: "s".into(), index: 0, block: table(vec![table(vec![FlowBlock::Image(node("cell-image", url, vec![]))])]) },
                Operation::AddPanel { index: 0, panel: FigurePanel {
                    id: "f".into(), bounds: Rect::default(), label: "A".into(), style: Style::default(),
                    body: PanelBody::Diagram(Diagram { nodes: vec![node("d", url, vec![])], edges: vec![], evidence_ids: BTreeMap::new(), node_origins: BTreeMap::new() }),
                } },
            ];
            let accepted: Vec<usize> = operations.iter().enumerate()
                .filter(|(_, operation)| operation.check_new_interaction_urls().map_err(|error| error.code) != Err(crate::ErrorCode::InvalidArgument))
                .map(|(index, _)| index).collect();
            assert_eq!(accepted, Vec::<usize>::new(), "operations not refused for {url:?}");
        }
        let plain = Operation::AddNode { page_id: "p".into(), parent_id: None, node: node("n", "https://example.com/", vec![node("c", "mailto:a@b", vec![])]) };
        plain.check_new_interaction_urls().unwrap();
        assert!(check_new_interaction_url("https://example.com/ a").is_err(), "the read rule still applies");
        assert!(check_new_interaction_url("https://example.com/\u{00AD}").is_err(), "a soft hyphen is a format character");
        check_new_interaction_url("https://example.com/\u{D55C}\u{AE00}").expect("other non-ASCII letters are allowed");
    }

    #[test]
    fn the_format_character_table_is_sorted_and_disjoint() {
        assert!(FORMAT_CHARACTERS.iter().all(|(first, last)| first <= last));
        assert!(FORMAT_CHARACTERS.windows(2).all(|pair| pair[0].1 < pair[1].0));
        assert_eq!(FORMAT_CHARACTERS.iter().map(|(first, last)| *last as u32 - *first as u32 + 1).sum::<u32>(), 170, "Unicode 15.1 has 170 Cf code points");
    }
}
