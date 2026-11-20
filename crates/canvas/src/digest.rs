//! Content digests. The core requires that design-only operations (geometry, style, layout, variant patches,
//! named styles, page size) leave `ContentDigest.content` unchanged. Text, list semantics, structure, page order,
//! notes, interactions, and web source changes must change it. Table width weights are design metadata and are excluded.

use designllm_contracts::{
    json_hash, sha256, CanvasBoardMemo, CanvasDocument, CanvasPageSection, CanvasTableRow, CanvasTableSpan, ContentDigest, DltResult, FigureLink, Interaction, Node, NodeKind,
    PageRole, RepeatSpec, ShowCondition, TextListKind, WebDocument,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
struct NodeView<'a> {
    id: &'a str,
    kind: NodeKind,
    text: &'a str,
    asset: Option<&'a str>,
    /// The extracted first-frame poster is visible video content, but absent from ordinary nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    video_poster_asset: Option<&'a str>,
    interactions: &'a [Interaction],
    /// Information fields (alternative text, layer name, visibility). Left out when unset, so a node without them keeps
    /// the digest it had before the fields existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    alt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden: Option<bool>,
    /// Omitted while unset, so listless node content keeps its existing digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    list: Option<TextListKind>,
    /// Omitted while unset, so a canvas without semantic tables keeps its existing digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    table: Option<TableContentView<'a>>,
    /// Present only for a materialized component instance; omitted on ordinary nodes to keep legacy digests stable.
    #[serde(skip_serializing_if = "Option::is_none")]
    instance_of: Option<&'a str>,
    /// Repeat group data binding and show condition (D21); omitted while unset so other nodes keep their digests.
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat: Option<&'a RepeatSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    condition: Option<&'a ShowCondition>,
    /// Source node ids an instance hides by a repeat condition; omitted when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden_overrides: Option<Vec<&'a str>>,
    children: Vec<NodeView<'a>>,
}

#[derive(Serialize)]
struct TableContentView<'a> {
    rows: &'a [CanvasTableRow],
    #[serde(skip_serializing_if = "Option::is_none")]
    header_row_id: Option<&'a str>,
    /// Merged cell spans change semantic table structure and therefore content. Keep absent when empty for legacy digest stability.
    #[serde(skip_serializing_if = "Option::is_none")]
    cell_spans: Option<&'a BTreeMap<String, CanvasTableSpan>>,
}

#[derive(Serialize)]
struct PageView<'a> {
    id: &'a str,
    notes: &'a str,
    /// Left out when unset, so a page without a name keeps its digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    /// Present only for a component source page; omitted when unset to preserve older digests.
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<PageRole>,
    nodes: Vec<NodeView<'a>>,
}

#[derive(Serialize)]
struct LinkView<'a> {
    figure_id: &'a str,
    figure_revision: u64,
    target_node_id: &'a str,
    representation: &'a str,
    render_hash: &'a str,
}

#[derive(Serialize)]
struct CanvasView<'a> {
    pages: Vec<PageView<'a>>,
    /// Board-only annotations are content; absent while empty so legacy digests remain unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    board_memos: Option<&'a [CanvasBoardMemo]>,
    /// Page grouping is content; absent while empty to preserve old digests.
    #[serde(skip_serializing_if = "Option::is_none")]
    page_sections: Option<&'a [CanvasPageSection]>,
    /// Left out when empty, so a canvas without figure links keeps the digest it had before links existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    figure_links: Vec<LinkView<'a>>,
}

fn node_view(node: &Node) -> NodeView<'_> {
    NodeView {
        id: &node.id,
        kind: node.kind,
        text: &node.text,
        asset: node.asset.as_ref().map(|a| a.sha256.as_str()),
        video_poster_asset: node.ext.video_poster_asset.as_ref().map(|asset| asset.sha256.as_str()),
        interactions: &node.interactions,
        alt: node.ext.alt.as_deref(),
        name: node.ext.name.as_deref(),
        hidden: node.ext.hidden,
        list: node.ext.list,
        table: node.ext.table.as_ref().map(|table| TableContentView {
            rows: &table.rows,
            header_row_id: table.header_row_id.as_deref(),
            cell_spans: (!table.cell_spans.is_empty()).then_some(&table.cell_spans),
        }),
        instance_of: node.ext.instance.as_ref().map(|instance| instance.component_page_id.as_str()),
        repeat: node.ext.repeat.as_ref(),
        condition: node.ext.condition.as_ref(),
        hidden_overrides: node.ext.instance.as_ref().map(|instance| {
            instance.overrides.iter().filter(|(_, over)| over.hidden == Some(true)).map(|(id, _)| id.as_str()).collect::<Vec<_>>()
        }).filter(|ids| !ids.is_empty()),
        children: node.children.iter().map(node_view).collect(),
    }
}

/// Digest of the canvas content: page order, page notes and names, component roles and instance source references, the node tree, node text and asset references,
/// alternative text, layer names, visibility, list semantics, video source/poster assets, table cell spans, board memos/titles, page sections, interactions, and figure links (target, revision,
/// representation, render hash). Bounds, styles, layout, variants, named styles, locks, image fits/crops, page sizes, page guides, and table width weights are excluded.
pub(crate) fn canvas_digest(doc: &CanvasDocument, links: &[FigureLink]) -> DltResult<ContentDigest> {
    let view = CanvasView {
        pages: doc
            .pages
            .iter()
            .map(|page| PageView {
                id: &page.id,
                notes: &page.notes,
                name: page.name.as_deref(),
                role: page.role,
                nodes: page.nodes.iter().map(node_view).collect(),
            })
            .collect(),
        board_memos: (!doc.board_memos.is_empty()).then_some(doc.board_memos.as_slice()),
        page_sections: (!doc.page_sections.is_empty()).then_some(doc.page_sections.as_slice()),
        figure_links: links
            .iter()
            .map(|link| LinkView {
                figure_id: &link.figure_id,
                figure_revision: link.figure_revision,
                target_node_id: &link.target_node_id,
                representation: &link.representation,
                render_hash: &link.render_hash,
            })
            .collect(),
    };
    Ok(ContentDigest { content: json_hash(&view)?, data: sha256(b"") })
}

#[derive(Serialize)]
struct WebView<'a> {
    entry: &'a str,
    files: Vec<(&'a str, &'a str)>,
}

/// Digest of a source project: entry name and every file path with its content hash. The design-token stylesheet in the
/// entry folder is left out while it is still the generator's output (`digest_excluded_path`), so a token value update keeps
/// the content digest. An edited, foreign or otherwise changed file at that path is included.
pub(crate) fn web_digest(doc: &WebDocument) -> DltResult<ContentDigest> {
    let excluded = crate::web::design_system::digest_excluded_path(doc);
    let view = WebView {
        entry: &doc.entry,
        files: doc
            .files
            .iter()
            .filter(|(path, _)| excluded.as_deref() != Some(path.as_str()))
            .map(|(path, file)| (path.as_str(), file.sha256.as_str()))
            .collect(),
    };
    Ok(ContentDigest { content: json_hash(&view)?, data: sha256(b"") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{AssetRef, CanvasBoardMemoKind, CanvasTable, CanvasTableRow, NodeExt, Page, PixelRect, Rect, Size, Style};

    fn table_document() -> CanvasDocument {
        let cell = |id: &str| Node {
            id: id.into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 100, height: 36 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 200, height: 36 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![cell("cell-1"), cell("cell-2")],
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: NodeExt {
                table: Some(CanvasTable {
                    rows: vec![CanvasTableRow { id: "row-1".into(), cell_ids: vec!["cell-1".into(), "cell-2".into()] }],
                    header_row_id: None,
                    column_widths: None,
                    cell_spans: BTreeMap::new(),
                }),
                ..NodeExt::default()
            },
        };
        CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("page", Size::slide()) }],
            ..CanvasDocument::default()
        }
    }

    #[test]
    fn table_spans_change_content_digest_and_unmerge_restores_it() {
        let mut document = table_document();
        let before = canvas_digest(&document, &[]).expect("unmerged table digest");
        document.pages[0].nodes[0].ext.table.as_mut().expect("table metadata")
            .cell_spans.insert("cell-1".into(), CanvasTableSpan { row_span: 1, column_span: 2 });

        let merged = canvas_digest(&document, &[]).expect("merged table digest");
        assert_ne!(merged.content, before.content, "a Content table merge changes semantic content");
        assert_eq!(merged.data, before.data, "table structure does not change the data digest");

        document.pages[0].nodes[0].ext.table.as_mut().expect("table metadata").cell_spans.clear();
        let unmerged = canvas_digest(&document, &[]).expect("unmerged table digest after clearing spans");
        assert_eq!(unmerged, before, "unmerge restores both digests exactly");
    }

    #[test]
    fn empty_table_spans_are_omitted_from_the_content_digest_view() {
        let document = table_document();
        let view = serde_json::to_value(node_view(&document.pages[0].nodes[0])).expect("node digest view");
        assert!(view["table"].get("cell_spans").is_none(), "empty spans preserve the legacy digest shape");
    }

    #[test]
    fn image_crop_is_design_metadata_and_does_not_change_content_digest() {
        let mut document = table_document();
        document.pages[0].nodes.push(Node {
            id: "image".into(), kind: NodeKind::Image, bounds: Rect { x: 10, y: 20, width: 300, height: 200 },
            text: String::new(), style: Style::default(),
            asset: Some(AssetRef { sha256: "a".repeat(64), mime: "image/png".into(), bytes: 1 }),
            children: Vec::new(), layout: None, interactions: Vec::new(), variants: BTreeMap::new(), ext: NodeExt::default(),
        });
        let before = canvas_digest(&document, &[]).expect("full source image digest");
        document.pages[0].nodes[1].ext.image_crop_pixel_rect = Some(PixelRect { x: 10, y: 20, width: 30, height: 40 });
        assert_eq!(canvas_digest(&document, &[]).expect("cropped image digest"), before, "crop is a design-only change");
        document.pages[0].nodes[1].ext.image_crop_pixel_rect = None;
        assert_eq!(canvas_digest(&document, &[]).expect("reset image crop digest"), before);
    }

    #[test]
    fn video_source_and_first_frame_poster_are_content_digest_inputs() {
        let mut document = table_document();
        document.pages[0].nodes.push(Node {
            id: "video".into(), kind: NodeKind::Video, bounds: Rect { x: 0, y: 0, width: 300, height: 200 },
            text: String::new(), style: Style::default(),
            asset: Some(AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 64 }),
            children: Vec::new(), layout: None, interactions: Vec::new(), variants: BTreeMap::new(),
            ext: NodeExt {
                video_poster_asset: Some(AssetRef { sha256: "b".repeat(64), mime: "image/png".into(), bytes: 32 }),
                ..Default::default()
            },
        });
        let before = canvas_digest(&document, &[]).expect("source and poster digest");
        document.pages[0].nodes[1].ext.video_poster_asset.as_mut().unwrap().sha256 = "c".repeat(64);
        let changed_poster = canvas_digest(&document, &[]).expect("changed poster digest");
        assert_ne!(changed_poster.content, before.content, "the extracted poster is visible video content");
        assert_eq!(changed_poster.data, before.data);
    }

    #[test]
    fn board_memos_change_content_digest_and_empty_memos_preserve_legacy_shape() {
        let mut document = table_document();
        let before = canvas_digest(&document, &[]).expect("digest without board memos");
        assert!(serde_json::to_value(&document).unwrap().get("board_memos").is_none(), "empty board memos are omitted from native data");

        document.board_memos.push(CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(),
            page_id: "page".into(),
            bounds: Rect { x: -100, y: 200, width: 300, height: 400 },
            text: "board only".into(),
            color: "#FFF2CC".into(),
        });
        let with_memo = canvas_digest(&document, &[]).expect("digest with board memo");
        assert_ne!(with_memo.content, before.content);
        assert_eq!(with_memo.data, before.data);

        document.board_memos.clear();
        assert_eq!(canvas_digest(&document, &[]).unwrap(), before, "removing a board memo restores legacy content digest");
    }

    #[test]
    fn page_guides_are_design_only_and_leave_content_digest_unchanged() {
        let mut document = table_document();
        let before = canvas_digest(&document, &[]).expect("digest without page guides");
        document.layout_guides.insert(
            "page".into(),
            designllm_contracts::CanvasPageGuides {
                columns: Some(designllm_contracts::CanvasAxisGuides {
                    count: 3,
                    spacing_emu: 20_000,
                    leading_margin_emu: 100_000,
                    trailing_margin_emu: 100_000,
                }),
                rows: None,
            },
        );
        assert_eq!(canvas_digest(&document, &[]).expect("digest with page guides"), before, "page guides are editor settings, not content");
    }

    #[test]
    fn title_and_page_sections_change_content_digest_but_empty_sections_preserve_legacy_shape() {
        let mut document = table_document();
        let before = canvas_digest(&document, &[]).expect("base content digest");
        let serialized = serde_json::to_value(&document).unwrap();
        assert!(serialized.get("page_sections").is_none());

        document.board_memos.push(CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Title,
            id: "title-1".into(),
            page_id: "page".into(),
            bounds: Rect { x: 0, y: -50_000, width: 300_000, height: 40_000 },
            text: "Page title".into(),
            color: "#223344".into(),
        });
        let with_title = canvas_digest(&document, &[]).expect("title content digest");
        assert_ne!(with_title.content, before.content);
        assert_eq!(with_title.data, before.data);

        document.board_memos.clear();
        document.page_sections.push(CanvasPageSection { id: "section-1".into(), name: "Part 1".into(), start_page_id: "page".into() });
        let with_section = canvas_digest(&document, &[]).expect("section content digest");
        assert_ne!(with_section.content, before.content);
        assert_eq!(with_section.data, before.data);
        document.page_sections.clear();
        assert_eq!(canvas_digest(&document, &[]).unwrap(), before, "removing all sections restores the legacy content digest");
    }

    /// A project whose entry is `index.html` and whose files are the given (path, content) pairs.
    fn project(files: &[(&str, &str)]) -> WebDocument {
        WebDocument {
            entry: "index.html".into(),
            files: files
                .iter()
                .map(|(path, content)| {
                    (path.to_string(), designllm_contracts::WebFile { sha256: sha256(content.as_bytes()), content: content.to_string() })
                })
                .collect(),
        }
    }

    #[test]
    fn web_digest_leaves_out_only_an_unedited_generated_stylesheet() {
        use crate::web::design_system::{generated_content, GENERATED_NAME};
        use designllm_contracts::{TokenDef, TokenValues};

        let tokens = BTreeMap::from([(
            "primary".to_owned(),
            TokenDef { name: "Primary".into(), values: TokenValues::Color { light: "#1F6FEB".into(), dark: "#8AB4FF".into() } },
        )]);
        let css = generated_content(&tokens).expect("generated stylesheet");
        let edited_css = css.replace("#1F6FEB", "#000000");
        let html = "<p>x</p>";
        let digest = |files: &[(&str, &str)]| web_digest(&project(files)).expect("web digest").content;

        let plain = digest(&[("index.html", html)]);
        assert_eq!(digest(&[("index.html", html), (GENERATED_NAME, css.as_str())]), plain, "an unedited stylesheet is left out");
        assert_ne!(digest(&[("index.html", html), (GENERATED_NAME, edited_css.as_str())]), plain, "an edited stylesheet is content");
        assert_ne!(digest(&[("index.html", html), (GENERATED_NAME, "body { color: red; }")]), plain, "a foreign file is content");
        assert_ne!(digest(&[("index.html", html), ("other/design-tokens.css", css.as_str())]), plain, "only the entry folder is left out");
    }

    #[test]
    fn web_digest_keeps_recomputed_headers_with_other_rules_in_the_content() {
        use crate::web::design_system::{generated_content, GENERATED_NAME};
        use designllm_contracts::{TokenDef, TokenValues};

        let tokens = BTreeMap::from([(
            "primary".to_owned(),
            TokenDef { name: "Primary".into(), values: TokenValues::Color { light: "#1F6FEB".into(), dark: "#8AB4FF".into() } },
        )]);
        let css = generated_content(&tokens).expect("generated stylesheet");
        let body = css.split_once('\n').expect("header line").1;
        let with_header = |body: &str| format!("/* designllm:design-tokens sha256={} */\n{body}", sha256(body.as_bytes()));
        let html = "<p>x</p>";
        let digest = |files: &[(&str, &str)]| web_digest(&project(files)).expect("web digest").content;
        let plain = digest(&[("index.html", html)]);

        // The header is recomputed, so only the body decides: anything beyond the generator's layout is content.
        for forged in [
            format!("{body}body {{ display: none; }}\n"),
            format!("{body}.label::before {{ content: \"x\"; }}\n"),
            format!("{body}:root {{\n  --dl-extra: #000000;\n}}\n"),
        ] {
            let file = with_header(&forged);
            assert_ne!(digest(&[("index.html", html), (GENERATED_NAME, file.as_str())]), plain, "{forged:?} must stay in the content");
        }
    }

    #[test]
    fn web_digest_leaves_out_escaped_values_and_keeps_other_spellings_in_the_content() {
        use crate::web::design_system::{generated_content, GENERATED_NAME};
        use designllm_contracts::{TokenDef, TokenValues};

        let tokens = BTreeMap::from([
            (
                "body-font".to_owned(),
                TokenDef {
                    name: "Body font".into(),
                    values: TokenValues::FontFamily {
                        light: "Quote \"q\" back\\slash; semi} <tag> Noto".into(),
                        dark: "Pretendard".into(),
                    },
                },
            ),
            ("gap".to_owned(), TokenDef { name: "Gap".into(), values: TokenValues::Spacing { light: 76_200, dark: 114_300 } }),
        ]);
        let css = generated_content(&tokens).expect("generated stylesheet");
        let body = css.split_once('\n').expect("header line").1;
        let with_header = |body: &str| format!("/* designllm:design-tokens sha256={} */\n{body}", sha256(body.as_bytes()));
        let html = "<p>x</p>";
        let digest = |files: &[(&str, &str)]| web_digest(&project(files)).expect("web digest").content;
        let plain = digest(&[("index.html", html)]);
        assert_eq!(digest(&[("index.html", html), (GENERATED_NAME, css.as_str())]), plain, "the generator's escaped text is left out");

        // Other spellings of the same values, with the header recomputed, are content.
        for other in [
            body.replace("\\3C ", "\\3c "),
            body.replace("\\3C ", "<"),
            body.replace("--dl-gap: 8px;", "--dl-gap: 8.5px;"),
        ] {
            let file = with_header(&other);
            assert_ne!(digest(&[("index.html", html), (GENERATED_NAME, file.as_str())]), plain, "{other:?} must stay in the content");
        }
    }
}
