//! Shared test support for the canvas crate.
//! `StubRenderer` is a deterministic font-free RenderProvider used until the shared Opus-2 renderer is available as a
//! dev-dependency. Metrics: Latin graphemes advance 0.5 em, CJK and Hangul graphemes 1 em, line height 1.2 em,
//! baseline 0.95 em below the top. Private-use code points are reported as missing glyphs. Every run reports the
//! font family "Pretendard", so requesting another family must produce FONT_SUBSTITUTED.
#![allow(dead_code)]

use designllm_contracts::{
    sha256, Artifact, ArtifactKind, AssetRef, AssetStore, CanvasDocument, CreateRequest, DltError, DltResult,
    EncodedOutput, Editability, EngineAdapter, EngineContext, ErrorCode, FontFaceRef, GlyphMetric, LayoutSpec, Node,
    NodeKind, Operation, Point, RenderPage, RenderProfile, RenderProvider, RenderScene, Rect, ShapedText, Size, Style,
    HitTestResult, TextPosition, TextRange, Interaction, EMU_PER_PT,
};
use std::collections::BTreeMap;
use std::sync::Mutex;
use unicode_segmentation::UnicodeSegmentation;

pub const PX: i64 = 9_525;

pub fn px(value: i64) -> i64 {
    value * PX
}

#[derive(Default)]
pub struct MemoryAssets {
    inner: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
}

impl AssetStore for MemoryAssets {
    fn put(&self, bytes: &[u8], mime: &str) -> DltResult<AssetRef> {
        let hash = sha256(bytes);
        self.inner.lock().unwrap().insert(hash.clone(), (bytes.to_vec(), mime.to_string()));
        Ok(AssetRef { sha256: hash, mime: mime.to_string(), bytes: bytes.len() as u64 })
    }

    fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
        self.inner
            .lock()
            .unwrap()
            .get(&asset.sha256)
            .map(|(bytes, _)| bytes.clone())
            .ok_or_else(|| DltError::new(ErrorCode::NotFound, "asset not found"))
    }

    fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
        self.inner
            .lock()
            .unwrap()
            .get(hash)
            .map(|(bytes, mime)| AssetRef { sha256: hash.to_string(), mime: mime.clone(), bytes: bytes.len() as u64 })
            .ok_or_else(|| DltError::new(ErrorCode::NotFound, "asset not found"))
    }
}

#[derive(Default)]
pub struct StubRenderer;

fn is_wide(g: &str) -> bool {
    g.chars().next().is_some_and(|c| matches!(c as u32, 0x1100..=0x11FF | 0x3040..=0x30FF | 0x3130..=0x318F | 0x4E00..=0x9FFF | 0xAC00..=0xD7A3))
}

fn is_missing(g: &str) -> bool {
    g.chars().next().is_some_and(|c| matches!(c as u32, 0xE000..=0xF8FF))
}

impl RenderProvider for StubRenderer {
    fn shape_text(&self, text: &str, style: &Style) -> DltResult<ShapedText> {
        let em = style.font_size;
        let ascent = em * 95 / 100;
        let descent = em * 25 / 100;
        let line_h = em * 12 / 10;
        let mut x = 0i64;
        let mut glyphs = Vec::new();
        let mut rects = Vec::new();
        let mut caret = vec![0i64];
        let mut missing = Vec::new();
        for (i, g) in text.graphemes(true).enumerate() {
            let adv = if is_wide(g) { em } else { em / 2 };
            if is_missing(g) {
                missing.push(g.to_string());
            }
            glyphs.push(GlyphMetric {
                glyph_id: i as u32,
                font_key: "stub:Pretendard:0".into(),
                position: Point { x, y: 0 },
                advance_x: adv,
                grapheme_start: i as u32,
                grapheme_end: i as u32 + 1,
            });
            rects.push(Rect { x, y: -ascent, width: adv, height: line_h });
            x += adv;
            caret.push(x);
        }
        let fonts_used = if text.is_empty() {
            Vec::new()
        } else {
            vec![FontFaceRef {
                key: "stub:Pretendard:0".into(),
                file_sha256: sha256(b"stub-pretendard-regular"),
                face_index: 0,
                postscript_name: "Pretendard-Regular".into(),
                family: "Pretendard".into(),
                variations: BTreeMap::new(),
            }]
        };
        Ok(ShapedText {
            size: Size { width: x, height: line_h },
            baseline: ascent,
            ascent,
            descent,
            line_gap: 0,
            cap_height: em * 7 / 10,
            glyphs,
            caret_x: caret,
            grapheme_rects: rects,
            fonts_used,
            missing_glyphs: missing,
        })
    }

    fn encode(&self, scene: &RenderScene, format: &str, _profile: &RenderProfile, _assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        let pages: Vec<u32> = scene.pages.iter().map(|p| p.page_index).collect();
        Ok(EncodedOutput {
            bytes: format!("stub-encode:{format}:pages={pages:?}").into_bytes(),
            mime: "application/octet-stream".into(),
            editability: Editability::Vector,
            diagnostics: Vec::new(),
        })
    }

    fn font_catalog_fingerprint(&self) -> String {
        "stub-catalog".into()
    }

    fn available_fonts(&self) -> Vec<String> {
        vec!["Pretendard".into()]
    }

    fn hit_test(&self, _page: &RenderPage, _point: Point) -> DltResult<Option<HitTestResult>> {
        Ok(None)
    }

    fn caret_rect(&self, _page: &RenderPage, _position: &TextPosition) -> DltResult<Rect> {
        Err(DltError::unsupported("stub renderer has no caret geometry"))
    }

    fn selection_rects(&self, _page: &RenderPage, _range: &TextRange) -> DltResult<Vec<Rect>> {
        Ok(Vec::new())
    }
}

pub fn default_style() -> Style {
    Style::default()
}

pub fn text_style(size_px: i64) -> Style {
    Style { font_size: px(size_px), ..Style::default() }
}

pub fn node(id: &str, kind: NodeKind, bounds: Rect) -> Node {
    Node {
        id: id.into(),
        kind,
        bounds,
        text: String::new(),
        style: Style::default(),
        asset: None,
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: BTreeMap::new(),
        ext: Default::default(),
    }
}

pub fn rect(x: i64, y: i64, w: i64, h: i64) -> Rect {
    Rect { x, y, width: w, height: h }
}

pub fn rect_px(x: i64, y: i64, w: i64, h: i64) -> Rect {
    rect(px(x), px(y), px(w), px(h))
}

pub fn text_node(id: &str, bounds: Rect, text: &str, style: Style) -> Node {
    Node { text: text.into(), style, ..node(id, NodeKind::Text, bounds) }
}

pub fn group(id: &str, bounds: Rect, layout: Option<LayoutSpec>, children: Vec<Node>) -> Node {
    Node { children, layout, ..node(id, NodeKind::Group, bounds) }
}

pub fn spec(mode: &str) -> LayoutSpec {
    LayoutSpec { mode: mode.into(), ..Default::default() }
}

pub fn image_node(id: &str, bounds: Rect, asset: AssetRef) -> Node {
    Node { asset: Some(asset), ..node(id, NodeKind::Image, bounds) }
}

pub fn ctx<'a>(assets: &'a dyn AssetStore, renderer: &'a dyn RenderProvider) -> EngineContext<'a> {
    EngineContext { assets, renderer }
}

pub fn create(engine: &dyn EngineAdapter, context: &EngineContext<'_>, title: &str, size: Option<Size>) -> Artifact {
    engine
        .create(
            &CreateRequest { kind: ArtifactKind::Canvas, title: title.into(), size, template: None },
            context,
        )
        .expect("create canvas")
}

pub fn page_id(artifact: &Artifact) -> String {
    match &artifact.body {
        designllm_contracts::ArtifactBody::Canvas(doc) => doc.pages[0].id.clone(),
        _ => panic!("not a canvas"),
    }
}

pub fn canvas_doc(artifact: &Artifact) -> &CanvasDocument {
    match &artifact.body {
        designllm_contracts::ArtifactBody::Canvas(doc) => doc,
        _ => panic!("not a canvas"),
    }
}

/// Applies operations the way the core does: prepare on a copy, then take the prepared artifact if it succeeded.
pub fn apply(engine: &dyn EngineAdapter, artifact: &Artifact, ops: &[Operation], context: &EngineContext<'_>) -> DltResult<Artifact> {
    engine.prepare_change(artifact, ops, context).map(|p| p.artifact)
}

pub fn set_size(artifact: &mut Artifact, size: Size) {
    if let designllm_contracts::ArtifactBody::Canvas(doc) = &mut artifact.body {
        doc.pages[0].size = size;
    }
}

pub fn interaction_navigate(page_id: &str) -> Interaction {
    Interaction::NavigatePage { page_id: page_id.into() }
}

/// A stored, uncompressed ZIP with the given (name, bytes) entries. Used to build fixtures for WebCode imports.
pub fn zip_stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let c = crc(data);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&c.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

pub fn emu_pt(value: f64) -> i64 {
    (value * EMU_PER_PT as f64).round() as i64
}
