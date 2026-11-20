//! Engine-specific mapping records stored in `NativeSource.mappings`.
//!
//! Each record ties a model id to its PresentationML source: slide part, shape id, relationship, and
//! paragraph/run structure. Records hold identifiers and formatting values only. They never hold package
//! bytes, so the artifact JSON stays small.

use std::collections::BTreeMap;

use designllm_contracts::{DltError, DltResult, ErrorCode, Style};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const KIND_PAGE: &str = "page";
pub const KIND_SHAPE: &str = "shape";
pub const KIND_PICTURE: &str = "picture";
pub const KIND_CONNECTOR: &str = "connector";
pub const ORIGIN_SOURCE: &str = "source";
pub const ORIGIN_ADDED: &str = "added";

fn is_false(value: &bool) -> bool { !*value }

/// Mapping of a slide page (`Page.id`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageMap {
    pub kind: String,
    pub origin: String,
    /// Slide part name, for example `ppt/slides/slide1.xml`. Allocated at prepare time for added pages.
    pub part: String,
    /// `p:sldId/@id`. Unique in the presentation and at least 256.
    pub sld_id: u32,
    /// Relationship id in `ppt/_rels/presentation.xml.rels`. Empty for added pages until the writer assigns one.
    pub rel_id: String,
    /// Notes slide part name when the page has notes, allocated at prepare time.
    pub notes_part: Option<String>,
    /// Largest `p:cNvPr/@id` in the slide, including unmodelled objects. New shapes use the next id.
    pub max_shape_id: u32,
}

/// Mapping of a shape, picture, or connector (`Node.id`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShapeMap {
    pub kind: String,
    pub origin: String,
    /// `p:cNvPr/@id` in the owning slide. Unique within the slide.
    pub cnvpr_id: u32,
    /// Preset geometry name, for example `rect`, `roundRect`, `ellipse`, `line`.
    pub geometry: String,
    /// True for `p:cNvSpPr/@txBox="1"`.
    pub text_box: bool,
    /// True when the source has a `p:txBody` (or the node was created with one).
    pub has_text_body: bool,
    pub paragraphs: Vec<ParagraphMap>,
    pub body: BodyMap,
    /// Media part for pictures.
    pub media_part: Option<String>,
    /// Relationship id used by `a:blip/@r:embed` for pictures.
    pub rel_id: Option<String>,
    pub flip_h: bool,
    pub flip_v: bool,
    /// `a:xfrm/@rot` in 60000ths of a degree.
    pub rotation: i64,
    /// True when a connector has a tail or head end.
    pub arrow: bool,
    /// Source endpoint markers whose OOXML types are outside the contract's Open/Filled mapping.
    /// They remain in the source XML and cannot be edited through StyleField.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unmapped_head_end: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unmapped_tail_end: bool,
    /// Endpoint size attributes are preserved in source XML but are not represented by LineEnd.
    #[serde(default, skip_serializing_if = "is_false")]
    pub custom_head_end_size: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub custom_tail_end_size: bool,
    /// Placeholder type when the shape is a placeholder (`title`, `body`, and so on).
    pub placeholder: Option<String>,
    /// Render features that this engine does not draw exactly (for example `rotation`, `crop`).
    pub render_notes: Vec<String>,
    /// Anchor of the shape element in its slide part. Used to drop preservation regions when the shape is removed.
    pub anchor: String,
}

/// Text body insets and layout, from `a:bodyPr`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BodyMap {
    /// Left, top, right, bottom insets in EMU.
    pub insets: [i64; 4],
    /// `t`, `ctr`, or `b`.
    pub anchor: String,
    /// True for `wrap="square"`, false for `wrap="none"`.
    pub wrap_square: bool,
}

impl Default for BodyMap {
    fn default() -> Self {
        Self { insets: [91_440, 45_720, 91_440, 45_720], anchor: "t".into(), wrap_square: true }
    }
}

/// One paragraph: alignment, spacing, and its segments in order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ParagraphMap {
    /// `left`, `center`, `right`, `justify`, or `distributed`.
    pub align: String,
    /// `a:spcBef` in EMU (stored as whole hundredths of a point times 127).
    pub space_before: i64,
    pub space_after: i64,
    /// `a:lnSpc/a:spcPct` in whole percent (100 = single).
    pub line_spacing_pct: u32,
    pub segments: Vec<SegmentMap>,
}

/// A run of characters with one style, a line break, or a protected inline element.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SegmentMap {
    /// `chars` Unicode scalar values that share `style`.
    Run { chars: u32, style: Style },
    /// `a:br`. One logical character, U+000B.
    Break,
    /// `a:fld` or any unmodelled paragraph child. One logical character, U+FFFC. Never edited as text.
    Atom { element: String, display: String, style: Style },
}

/// Converts a mapping record to JSON for storage.
pub fn to_value<T: Serialize>(value: &T) -> DltResult<Value> {
    serde_json::to_value(value).map_err(|e| DltError::new(ErrorCode::Internal, e.to_string()))
}

/// Reads a mapping record from JSON, reporting the id when it is malformed.
pub fn from_value<T: for<'de> Deserialize<'de>>(id: &str, value: &Value) -> DltResult<T> {
    serde_json::from_value(value.clone())
        .map_err(|e| DltError::new(ErrorCode::IntegrityError, format!("mapping {id} is malformed: {e}")))
}

/// Mapping table: model id to record JSON. Keys are page ids and node ids.
pub type Mappings = BTreeMap<String, Value>;
