//! Shared encoder helpers: diagnostics, glyph face lookup, shape names, arrow heads.
use crate::fonts::{Face, FontBook};
use crate::paint::{parse_color, Rgba};
use designllm_contracts::*;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

pub(crate) struct Diags { list: Vec<Diagnostic>, seen: BTreeSet<(String, Option<String>, String)> }

impl Diags {
    /// Start with the scene's own diagnostics so layout losses reach the export report.
    pub fn new(scene: &[Diagnostic]) -> Self {
        let mut d = Self { list: Vec::new(), seen: BTreeSet::new() };
        for x in scene { d.push(x.clone()); }
        d
    }
    fn push(&mut self, d: Diagnostic) {
        if self.seen.insert((d.code.clone(), d.node_id.clone(), d.message.clone())) { self.list.push(d); }
    }
    pub fn add(&mut self, code: &str, message: impl Into<String>, node: Option<&str>, blocking: bool) {
        self.push(Diagnostic { code: code.into(), message: message.into(), node_id: node.map(str::to_string), blocking, params: Default::default() });
    }
    pub fn has_code(&self, code: &str) -> bool { self.list.iter().any(|d| d.code == code) }
    /// Parse a paint; unsupported values become a diagnostic and paint nothing.
    pub fn color(&mut self, value: &str, node: &str) -> Option<Rgba> {
        match parse_color(value) {
            Ok(c) => c,
            Err(()) => { self.add("unsupported_color", format!("Color '{value}' is not supported and is not painted"), Some(node), true); None }
        }
    }
    pub fn into_vec(self) -> Vec<Diagnostic> { self.list }
}

/// Resolves glyph font keys to faces through the primitive's `fonts_used` list.
pub(crate) struct GlyphSource<'a> { pub book: &'a FontBook, cache: RefCell<HashMap<String, Arc<Face>>> }

impl<'a> GlyphSource<'a> {
    pub fn new(book: &'a FontBook) -> Self { Self { book, cache: RefCell::new(HashMap::new()) } }
    pub fn face(&self, shaped: &ShapedText, key: &str) -> DltResult<Arc<Face>> {
        if let Some(f) = self.cache.borrow().get(key) { return Ok(f.clone()); }
        let face_ref = shaped.fonts_used.iter().find(|f| f.key == key).ok_or_else(|| DltError::invalid(format!("Glyph font key {key} is not listed in fonts_used")))?;
        let face = self.book.face_for_ref(face_ref)?;
        self.cache.borrow_mut().insert(key.to_string(), face.clone());
        Ok(face)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ShapeKind { Rect, Ellipse, RoundRect, Unknown }

impl ShapeKind {
    pub fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "rect" | "rectangle" | "box" | "cell" => Self::Rect,
            "ellipse" | "circle" | "oval" => Self::Ellipse,
            "round_rect" | "rounded_rect" | "roundrect" | "rounded_rectangle" => Self::RoundRect,
            _ => Self::Unknown,
        }
    }
}

/// Filled triangle at `to` pointing along from->to, in EMU.
pub(crate) fn arrow_head(from: Point, to: Point, stroke_width: i64) -> Option<[Point; 3]> {
    let (dx, dy) = ((to.x - from.x) as f64, (to.y - from.y) as f64);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 { return None; }
    let (ux, uy) = (dx / len, dy / len);
    let head = (stroke_width.max(EMU_PER_PT) as f64 * 4.0).max(4.0 * EMU_PER_PT as f64).min(len);
    let half = head * 0.35;
    let (bx, by) = (to.x as f64 - ux * head, to.y as f64 - uy * head);
    Some([
        to,
        Point { x: (bx - uy * half).round() as i64, y: (by + ux * half).round() as i64 },
        Point { x: (bx + uy * half).round() as i64, y: (by - ux * half).round() as i64 },
    ])
}

/// One line-end marker with its tip first and two rear vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub struct LineMarker {
    pub kind: LineEnd,
    pub points: [Point; 3],
}

/// Effective line markers in from/to order. An explicit to marker overrides the legacy arrow flag.
#[doc(hidden)]
pub fn line_markers(from: Point, to: Point, style: &Style, legacy_arrow: bool) -> [Option<LineMarker>; 2] {
    let start = style.stroke_start.and_then(|kind| {
        arrow_head(to, from, style.stroke_width).map(|points| LineMarker { kind, points })
    });
    let end_kind = style.stroke_end.or(if legacy_arrow { Some(LineEnd::Filled) } else { None });
    let end = end_kind.and_then(|kind| {
        arrow_head(from, to, style.stroke_width).map(|points| LineMarker { kind, points })
    });
    [start, end]
}
/// Fingerprint of faces referenced by Text primitives (same definition as designllm-flow).
pub fn scene_font_fingerprint(pages: &[RenderPage]) -> String {
    let mut faces = BTreeSet::new();
    for page in pages {
        for primitive in &page.primitives {
            if let Primitive::Text { shaped, .. } = primitive {
                for f in &shaped.fonts_used {
                    let variations: Vec<String> = f.variations.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    faces.insert(format!("{}#{}:{};{}", f.file_sha256, f.face_index, f.postscript_name, variations.join(",")));
                }
            }
        }
    }
    sha256(faces.into_iter().collect::<Vec<_>>().join("\n").as_bytes())
}
