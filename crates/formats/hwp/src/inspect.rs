//! Independent HWP 5.x container and record parser (written from the public Hancom HWP 5.0
//! format document, revision 1.3, without rhwp code). It re-reads saved output so a save is
//! never verified only by the library that wrote it.
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

pub const TAG_BEGIN: u16 = 0x010;
pub const TAG_DOCUMENT_PROPERTIES: u16 = TAG_BEGIN;
pub const TAG_ID_MAPPINGS: u16 = TAG_BEGIN + 1;
pub const TAG_BIN_DATA: u16 = TAG_BEGIN + 2;
pub const TAG_FACE_NAME: u16 = TAG_BEGIN + 3;
pub const TAG_BORDER_FILL: u16 = TAG_BEGIN + 4;
pub const TAG_CHAR_SHAPE: u16 = TAG_BEGIN + 5;
pub const TAG_TAB_DEF: u16 = TAG_BEGIN + 6;
pub const TAG_NUMBERING: u16 = TAG_BEGIN + 7;
pub const TAG_BULLET: u16 = TAG_BEGIN + 8;
pub const TAG_PARA_SHAPE: u16 = TAG_BEGIN + 9;
pub const TAG_STYLE: u16 = TAG_BEGIN + 10;
pub const TAG_PARA_HEADER: u16 = TAG_BEGIN + 50;
pub const TAG_PARA_TEXT: u16 = TAG_BEGIN + 51;
pub const TAG_PARA_CHAR_SHAPE: u16 = TAG_BEGIN + 52;
pub const TAG_PARA_LINE_SEG: u16 = TAG_BEGIN + 53;
pub const TAG_PARA_RANGE_TAG: u16 = TAG_BEGIN + 54;
pub const TAG_CTRL_HEADER: u16 = TAG_BEGIN + 55;
pub const TAG_LIST_HEADER: u16 = TAG_BEGIN + 56;
pub const TAG_PAGE_DEF: u16 = TAG_BEGIN + 57;
pub const TAG_SHAPE_COMPONENT: u16 = TAG_BEGIN + 60;
pub const TAG_TABLE: u16 = TAG_BEGIN + 61;
pub const TAG_SHAPE_COMPONENT_PICTURE: u16 = TAG_BEGIN + 69;

/// HWP 5.x FileHeader flags (little-endian u32 at byte offset 36).
pub const FILE_FLAG_COMPRESSED: u32 = 1 << 0;
pub const FILE_FLAG_ENCRYPTED: u32 = 1 << 1;
pub const FILE_FLAG_DISTRIBUTION: u32 = 1 << 2;
pub const FILE_FLAG_DRM: u32 = 1 << 4;
pub const FILE_FLAG_DIGITAL_SIGNATURE: u32 = 1 << 7;
pub const FILE_FLAG_CERTIFICATE_ENCRYPTED: u32 = 1 << 8;
pub const FILE_FLAG_SIGNATURE_RESERVED: u32 = 1 << 9;
pub const FILE_FLAG_CERTIFICATE_DRM: u32 = 1 << 10;
pub const FILE_FLAG_PERSONAL_INFO_SECURITY: u32 = 1 << 13;
pub const DIGITAL_SIGNATURE_STREAM: &str = "/DocOptions/DigitalSignature";

pub fn tag_name(tag: u16) -> &'static str {
    match tag {
        16 => "DOCUMENT_PROPERTIES", 17 => "ID_MAPPINGS", 18 => "BIN_DATA", 19 => "FACE_NAME", 20 => "BORDER_FILL",
        21 => "CHAR_SHAPE", 22 => "TAB_DEF", 23 => "NUMBERING", 24 => "BULLET", 25 => "PARA_SHAPE", 26 => "STYLE",
        27 => "DOC_DATA", 28 => "DISTRIBUTE_DOC_DATA", 30 => "COMPATIBLE_DOCUMENT", 31 => "LAYOUT_COMPATIBILITY",
        32 => "TRACKCHANGE", 66 => "PARA_HEADER", 67 => "PARA_TEXT", 68 => "PARA_CHAR_SHAPE", 69 => "PARA_LINE_SEG",
        70 => "PARA_RANGE_TAG", 71 => "CTRL_HEADER", 72 => "LIST_HEADER", 73 => "PAGE_DEF", 74 => "FOOTNOTE_SHAPE",
        75 => "PAGE_BORDER_FILL", 76 => "SHAPE_COMPONENT", 77 => "TABLE", 78 => "SHAPE_COMPONENT_LINE",
        79 => "SHAPE_COMPONENT_RECTANGLE", 80 => "SHAPE_COMPONENT_ELLIPSE", 81 => "SHAPE_COMPONENT_ARC",
        82 => "SHAPE_COMPONENT_POLYGON", 83 => "SHAPE_COMPONENT_CURVE", 84 => "SHAPE_COMPONENT_OLE",
        85 => "SHAPE_COMPONENT_PICTURE", 86 => "SHAPE_COMPONENT_CONTAINER", 87 => "CTRL_DATA", 88 => "EQEDIT",
        90 => "SHAPE_COMPONENT_TEXTART", 91 => "FORM_OBJECT", 92 => "MEMO_SHAPE", 93 => "MEMO_LIST",
        94 => "FORBIDDEN_CHAR", 95 => "CHART_DATA", 96 => "TRACK_CHANGE", 97 => "TRACK_CHANGE_AUTHOR",
        98 => "VIDEO_DATA", 99 => "SHAPE_COMPONENT_UNKNOWN", _ => "OTHER",
    }
}

#[derive(Debug, Clone)]
pub struct FileHeaderInfo { pub signature_ok: bool, pub version: [u8; 4], pub flags: u32 }
impl FileHeaderInfo {
    pub fn compressed(&self) -> bool { self.flags & FILE_FLAG_COMPRESSED != 0 }
    pub fn encrypted(&self) -> bool { self.flags & FILE_FLAG_ENCRYPTED != 0 }
    pub fn distribution(&self) -> bool { self.flags & FILE_FLAG_DISTRIBUTION != 0 }
    pub fn drm(&self) -> bool { self.flags & FILE_FLAG_DRM != 0 }
    pub fn digital_signature(&self) -> bool { self.flags & FILE_FLAG_DIGITAL_SIGNATURE != 0 }
    pub fn certificate_encrypted(&self) -> bool { self.flags & FILE_FLAG_CERTIFICATE_ENCRYPTED != 0 }
    pub fn signature_reserved(&self) -> bool { self.flags & FILE_FLAG_SIGNATURE_RESERVED != 0 }
    pub fn certificate_drm(&self) -> bool { self.flags & FILE_FLAG_CERTIFICATE_DRM != 0 }
    pub fn personal_info_security(&self) -> bool { self.flags & FILE_FLAG_PERSONAL_INFO_SECURITY != 0 }
    pub fn has_protected_markers(&self) -> bool {
        self.encrypted() || self.distribution() || self.drm() || self.certificate_encrypted() || self.certificate_drm() || self.personal_info_security()
    }
    pub fn has_signature_markers(&self) -> bool { self.digital_signature() || self.signature_reserved() }
    pub fn version_string(&self) -> String { format!("{}.{}.{}.{}", self.version[0], self.version[1], self.version[2], self.version[3]) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record { pub tag: u16, pub level: u16, pub payload: Vec<u8> }

#[derive(Debug, Clone)]
pub struct StreamInfo { pub raw: Vec<u8>, pub decoded: Option<Vec<u8>> }

#[derive(Debug, Clone)]
pub struct Hwp5 {
    pub header: FileHeaderInfo,
    pub strict_cfb: Result<(), String>,
    pub streams: BTreeMap<String, StreamInfo>,
    pub records: BTreeMap<String, Vec<Record>>,
}

pub fn is_cfb(bytes: &[u8]) -> bool { bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) }

/// Reads only the HWP FileHeader and checks whether the digital-signature stream exists.
pub fn read_file_header(bytes: &[u8]) -> Result<(FileHeaderInfo, bool), String> {
    if !is_cfb(bytes) { return Err("not an OLE compound file (HWP 5.x files are CFB containers)".into()); }
    let mut comp = cfb::CompoundFile::open(Cursor::new(bytes)).map_err(|e| format!("not a CFB file: {e}"))?;
    let raw = {
        let mut stream = comp.open_stream("/FileHeader").map_err(|e| format!("open /FileHeader: {e}"))?;
        let mut raw = [0; 40];
        stream.read_exact(&mut raw).map_err(|e| format!("FileHeader shorter than 40 bytes: {e}"))?;
        raw
    };
    let signature_ok = raw[..32].starts_with(b"HWP Document File");
    let v = u32::from_le_bytes(raw[32..36].try_into().unwrap());
    let header = FileHeaderInfo {
        signature_ok,
        version: [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8],
        flags: u32::from_le_bytes(raw[36..40].try_into().unwrap()),
    };
    let has_signature_stream = match comp.open_stream(DIGITAL_SIGNATURE_STREAM) {
        Ok(stream) => { drop(stream); true }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("digital signature stream lookup failed: {error}")),
    };
    Ok((header, has_signature_stream))
}

fn is_record_stream(path: &str) -> bool { path == "/DocInfo" || path.starts_with("/BodyText/Section") }

fn inflate_raw(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data).take(512 * 1024 * 1024).read_to_end(&mut out).map_err(|e| format!("raw deflate failed: {e}"))?;
    Ok(out)
}

pub fn parse_records(data: &[u8]) -> Result<Vec<Record>, String> {
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < data.len() {
        if i + 4 > data.len() { return Err(format!("truncated record header at byte {i}")); }
        let h = u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
        i += 4;
        let tag = (h & 0x3FF) as u16;
        let level = ((h >> 10) & 0x3FF) as u16;
        let mut size = (h >> 20) as usize;
        if size == 0xFFF {
            if i + 4 > data.len() { return Err(format!("truncated extended size at byte {i}")); }
            size = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
        }
        if i + size > data.len() { return Err(format!("record tag {tag} at byte {} declares {size} bytes, stream has {}", i - 4, data.len() - i)); }
        out.push(Record { tag, level, payload: data[i..i + size].to_vec() });
        i += size;
    }
    Ok(out)
}

pub fn open(bytes: &[u8]) -> Result<Hwp5, String> {
    if !is_cfb(bytes) { return Err("not an OLE compound file (HWP 5.x files are CFB containers)".into()); }
    let strict_cfb = cfb::CompoundFile::open_strict(Cursor::new(bytes)).map(|_| ()).map_err(|e| e.to_string());
    let mut comp = cfb::CompoundFile::open(Cursor::new(bytes)).map_err(|e| format!("not a CFB file: {e}"))?;
    let paths: Vec<String> = comp.walk().filter(|e| e.is_stream()).map(|e| e.path().to_string_lossy().replace('\\', "/")).collect();
    let mut streams = BTreeMap::new();
    for p in paths {
        let mut raw = Vec::new();
        comp.open_stream(&p).and_then(|mut s| s.read_to_end(&mut raw)).map_err(|e| format!("read stream {p}: {e}"))?;
        streams.insert(p, StreamInfo { raw, decoded: None });
    }
    let fh = streams.get("/FileHeader").ok_or_else(|| "missing /FileHeader".to_string())?;
    if fh.raw.len() < 40 { return Err("FileHeader shorter than 40 bytes".into()); }
    let signature_ok = fh.raw[..32].starts_with(b"HWP Document File");
    let v = u32::from_le_bytes(fh.raw[32..36].try_into().unwrap());
    let header = FileHeaderInfo { signature_ok, version: [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8], flags: u32::from_le_bytes(fh.raw[36..40].try_into().unwrap()) };
    let mut records = BTreeMap::new();
    let signature_stream = match comp.open_stream(DIGITAL_SIGNATURE_STREAM) {
        Ok(stream) => { drop(stream); true }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("digital signature stream lookup failed: {error}")),
    };
    if signature_ok && !header.has_protected_markers() && !header.has_signature_markers() && !signature_stream {
        let keys: Vec<String> = streams.keys().filter(|k| is_record_stream(k)).cloned().collect();
        for k in keys {
            let s = streams.get_mut(&k).unwrap();
            let data = if header.compressed() { let d = inflate_raw(&s.raw).map_err(|e| format!("{k}: {e}"))?; s.decoded = Some(d.clone()); d } else { s.raw.clone() };
            records.insert(k.clone(), parse_records(&data).map_err(|e| format!("{k}: {e}"))?);
        }
    }
    Ok(Hwp5 { header, strict_cfb, streams, records })
}

/// Record subtree by nesting level: node = record index + children.
#[derive(Debug, Clone)]
pub struct RecNode { pub index: usize, pub children: Vec<RecNode> }

pub fn build_tree(records: &[Record]) -> Result<Vec<RecNode>, String> {
    fn build(records: &[Record], i: &mut usize, level: u16) -> Result<Vec<RecNode>, String> {
        let mut out = Vec::new();
        while *i < records.len() {
            let r = &records[*i];
            if r.level < level { break; }
            if r.level > level { return Err(format!("record {} (tag {}) jumps from level {} to {}", *i, r.tag, level, r.level)); }
            let index = *i;
            *i += 1;
            let children = build(records, i, level + 1)?;
            out.push(RecNode { index, children });
        }
        Ok(out)
    }
    let mut i = 0;
    let out = build(records, &mut i, 0)?;
    if i != records.len() { return Err(format!("records after index {i} do not belong to the tree")); }
    Ok(out)
}

/// Units of a PARA_TEXT payload: visible characters and 8-unit controls (with raw units).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextUnit { Char(char), Ctrl { code: u16, id: u32, raw: Vec<u16> }, Mark(u16) }

pub fn is_single_unit(code: u16) -> bool { matches!(code, 0 | 10 | 13 | 24..=31) }

/// Extended controls own a CTRL_HEADER record; inline controls do not.
pub fn is_extended(code: u16) -> bool { matches!(code, 1..=3 | 11 | 12 | 14..=18 | 21..=23) }

pub fn text_units(payload: &[u8]) -> Vec<TextUnit> {
    let units: Vec<u16> = payload.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if u > 31 {
            if (0xD800..0xDC00).contains(&u) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
                let c = 0x10000 + (((u32::from(u) - 0xD800) << 10) | (u32::from(units[i + 1]) - 0xDC00));
                out.push(TextUnit::Char(char::from_u32(c).unwrap_or('\u{FFFD}')));
                i += 2;
            } else {
                out.push(TextUnit::Char(char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}')));
                i += 1;
            }
            continue;
        }
        if is_single_unit(u) {
            out.push(TextUnit::Mark(u));
            i += 1;
        } else {
            let end = (i + 8).min(units.len());
            let raw = units[i..end].to_vec();
            let id = if i + 2 < units.len() { (u32::from(units[i + 2]) << 16) | u32::from(units[i + 1]) } else { 0 };
            out.push(TextUnit::Ctrl { code: u, id, raw });
            i += 8;
        }
    }
    out
}

pub fn ctrl_id_string(id: u32) -> String {
    id.to_be_bytes().iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '?' }).collect()
}

/// PARA_HEADER fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParaHeader { pub nchars: u32, pub control_mask: u32, pub para_shape_id: u16, pub style_id: u8, pub break_type: u8, pub char_shape_count: u16, pub range_tag_count: u16, pub line_seg_count: u16 }

pub fn para_header(p: &[u8]) -> Option<ParaHeader> {
    if p.len() < 18 { return None; }
    Some(ParaHeader {
        nchars: u32::from_le_bytes(p[0..4].try_into().ok()?) & 0x7fff_ffff,
        control_mask: u32::from_le_bytes(p[4..8].try_into().ok()?),
        para_shape_id: u16::from_le_bytes([p[8], p[9]]),
        style_id: p[10],
        break_type: p[11],
        char_shape_count: u16::from_le_bytes([p[12], p[13]]),
        range_tag_count: u16::from_le_bytes([p[14], p[15]]),
        line_seg_count: u16::from_le_bytes([p[16], p[17]]),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharShapeView { pub face_ids: [u16; 7], pub relative_sizes: [u8; 7], pub base_size: i32, pub attr: u32, pub text_color: u32 }
impl CharShapeView {
    pub fn italic(&self) -> bool { self.attr & 1 != 0 }
    pub fn bold(&self) -> bool { self.attr & 2 != 0 }
}

pub fn parse_char_shape(p: &[u8]) -> Option<CharShapeView> {
    if p.len() < 56 { return None; }
    let mut face_ids = [0u16; 7];
    for (k, f) in face_ids.iter_mut().enumerate() { *f = u16::from_le_bytes([p[2 * k], p[2 * k + 1]]); }
    let mut relative_sizes = [0u8; 7];
    relative_sizes.copy_from_slice(&p[28..35]);
    Some(CharShapeView { face_ids, relative_sizes, base_size: i32::from_le_bytes(p[42..46].try_into().ok()?), attr: u32::from_le_bytes(p[46..50].try_into().ok()?), text_color: u32::from_le_bytes(p[52..56].try_into().ok()?) })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParaShapeView { pub attr1: u32, pub margin_left: i32, pub margin_right: i32, pub indent: i32, pub spacing_before: i32, pub spacing_after: i32, pub line_spacing_old: i32, pub attr3: Option<u32>, pub line_spacing: Option<u32> }
impl ParaShapeView {
    /// 0 justify, 1 left, 2 right, 3 center, 4 distribute, 5 split.
    pub fn alignment(&self) -> u32 { (self.attr1 >> 2) & 0x7 }
    /// 0 percent, 1 fixed, 2 space only, 3 minimum (5.0.2.5+ attr3, else attr1 bits 0..1).
    pub fn line_spacing_type(&self) -> u32 { self.attr3.map(|a| a & 0x1F).unwrap_or(self.attr1 & 0x3) }
    pub fn line_spacing_value(&self) -> i64 { self.line_spacing.map(i64::from).unwrap_or(i64::from(self.line_spacing_old)) }
}

pub fn parse_para_shape(p: &[u8]) -> Option<ParaShapeView> {
    if p.len() < 28 { return None; }
    let i32_at = |o: usize| i32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    Some(ParaShapeView {
        attr1: u32::from_le_bytes(p[0..4].try_into().ok()?),
        margin_left: i32_at(4), margin_right: i32_at(8), indent: i32_at(12), spacing_before: i32_at(16), spacing_after: i32_at(20), line_spacing_old: i32_at(24),
        attr3: p.get(46..50).map(|b| u32::from_le_bytes(b.try_into().unwrap())),
        line_spacing: p.get(50..54).map(|b| u32::from_le_bytes(b.try_into().unwrap())),
    })
}

/// FACE_NAME name.
pub fn parse_face_name(p: &[u8]) -> Option<String> {
    if p.len() < 3 { return None; }
    let n = u16::from_le_bytes([p[1], p[2]]) as usize;
    let units: Vec<u16> = p.get(3..3 + 2 * n)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

/// STYLE record (표 47): local and English names, kind (attr bits 0-2: 0 paragraph, 1 character),
/// next style, language, paragraph and character shape ids. `char_shape_at` is the byte offset
/// of the character shape id; `tail` holds the bytes after it (undocumented in revision 1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleView { pub local_name: String, pub english_name: String, pub attr: u8, pub next_style: u8, pub lang: i16, pub para_shape_id: u16, pub char_shape_id: u16, pub char_shape_at: usize, pub tail: Vec<u8> }
impl StyleView { pub fn kind(&self) -> u8 { self.attr & 0x7 } }

pub fn parse_style(p: &[u8]) -> Option<StyleView> {
    let local_name = utf16_at(p, 0)?;
    let at = 2 + 2 * u16::from_le_bytes([p[0], p[1]]) as usize;
    let english_name = utf16_at(p, at)?;
    let at = at + 2 + 2 * u16::from_le_bytes([p[at], p[at + 1]]) as usize;
    let f = p.get(at..at + 8)?;
    Some(StyleView {
        local_name, english_name, attr: f[0], next_style: f[1], lang: i16::from_le_bytes([f[2], f[3]]),
        para_shape_id: u16::from_le_bytes([f[4], f[5]]), char_shape_id: u16::from_le_bytes([f[6], f[7]]),
        char_shape_at: at + 6, tail: p[at + 8..].to_vec(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageDefView { pub width: u32, pub height: u32, pub margin_left: u32, pub margin_right: u32, pub margin_top: u32, pub margin_bottom: u32, pub margin_header: u32, pub margin_footer: u32, pub margin_gutter: u32, pub attr: u32 }
impl PageDefView { pub fn landscape(&self) -> bool { self.attr & 1 != 0 } }

pub fn parse_page_def(p: &[u8]) -> Option<PageDefView> {
    if p.len() < 40 { return None; }
    let u = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    Some(PageDefView { width: u(0), height: u(4), margin_left: u(8), margin_right: u(12), margin_top: u(16), margin_bottom: u(20), margin_header: u(24), margin_footer: u(28), margin_gutter: u(32), attr: u(36) })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableView { pub rows: u16, pub cols: u16 }
pub fn parse_table(p: &[u8]) -> Option<TableView> {
    if p.len() < 8 { return None; }
    Some(TableView { rows: u16::from_le_bytes([p[4], p[5]]), cols: u16::from_le_bytes([p[6], p[7]]) })
}

/// Table cell LIST_HEADER: paragraph count, then cell address/span/size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView { pub paragraphs: u16, pub col: u16, pub row: u16, pub col_span: u16, pub row_span: u16, pub width: u32, pub height: u32 }
pub fn parse_cell(p: &[u8]) -> Option<CellView> {
    if p.len() < 24 { return None; }
    let u16_at = |o: usize| u16::from_le_bytes([p[o], p[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    Some(CellView { paragraphs: u16_at(0), col: u16_at(8), row: u16_at(10), col_span: u16_at(12), row_span: u16_at(14), width: u32_at(16), height: u32_at(20) })
}

/// Picture BinData id inside SHAPE_COMPONENT_PICTURE.
pub fn picture_bin_id(p: &[u8]) -> Option<u16> { p.get(71..73).map(|b| u16::from_le_bytes([b[0], b[1]])) }

/// DocInfo BIN_DATA entry (표 17). `attr` bits 0-3: type (0 link, 1 embedding, 2 storage);
/// bits 4-5: compression (0 document default, 1 always, 2 never).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinDataEntry { pub attr: u16, pub storage_id: u16, pub extension: String }
impl BinDataEntry {
    pub fn kind(&self) -> u16 { self.attr & 0xF }
    pub fn compression(&self) -> u16 { (self.attr >> 4) & 0x3 }
    /// Storage stream of an embedded item: `/BinData/BIN%04X.ext`.
    pub fn stream_path(&self) -> String { format!("/BinData/BIN{:04X}.{}", self.storage_id, self.extension) }
}

fn utf16_at(p: &[u8], at: usize) -> Option<String> {
    let n = u16::from_le_bytes([*p.get(at)?, *p.get(at + 1)?]) as usize;
    let units: Vec<u16> = p.get(at + 2..at + 2 + 2 * n)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

/// BIN_DATA record; None for link entries (no storage) and malformed records.
pub fn parse_bin_data(p: &[u8]) -> Option<BinDataEntry> {
    let attr = u16::from_le_bytes([*p.first()?, *p.get(1)?]);
    if attr & 0xF == 0 { return None; }
    let storage_id = u16::from_le_bytes([*p.get(2)?, *p.get(3)?]);
    let extension = if attr & 0xF == 1 { utf16_at(p, 4)? } else { String::new() };
    Some(BinDataEntry { attr, storage_id, extension })
}

/// Fields of one picture control: gso CTRL_HEADER, its SHAPE_COMPONENT and SHAPE_COMPONENT_PICTURE.
#[derive(Debug, Clone, PartialEq)]
pub struct PictureRecord {
    /// CTRL_HEADER object attributes (bit 0: treat as character) and frame size (HWPUNIT).
    pub attr: u32, pub width: u32, pub height: u32,
    /// SHAPE_COMPONENT original and current size, flip word, rotation and its centre.
    pub original: (u32, u32), pub current: (u32, u32), pub flip: u32, pub rotation: i16, pub rotation_center: (i32, i32),
    /// Rendering matrices (translation, then scale/rotation pairs), 6 values each.
    pub matrices: Vec<[f64; 6]>,
    /// SHAPE_COMPONENT_PICTURE corner points (x0,y0..x3,y3), crop rectangle, BinData position and
    /// the image extent of the tail (when the tail has no effect data).
    pub points: [i32; 8], pub crop: [i32; 4], pub bin_id: u16, pub dims: Option<(u32, u32)>,
}

/// Picture fields of a `gso ` CTRL_HEADER subtree (None when it is not a picture control).
pub fn picture_record(recs: &[Record], ctrl: &RecNode) -> Option<PictureRecord> {
    let h = &recs[ctrl.index];
    if h.tag != TAG_CTRL_HEADER || h.payload.get(0..4) != Some(&b" osg"[..]) { return None; }
    let u32_at = |p: &[u8], o: usize| p.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let i32_at = |p: &[u8], o: usize| p.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap()));
    let comp_node = ctrl.children.iter().find(|c| recs[c.index].tag == TAG_SHAPE_COMPONENT)?;
    let pic_node = comp_node.children.iter().find(|c| recs[c.index].tag == TAG_SHAPE_COMPONENT_PICTURE)?;
    let c = &recs[comp_node.index].payload;
    let base = if c.len() >= 8 && c[0..4] == c[4..8] { 8 } else { 4 };
    let count = u16::from_le_bytes([*c.get(base + 42)?, *c.get(base + 43)?]) as usize;
    let mut matrices = Vec::new();
    for m in 0..(1 + 2 * count) {
        let at = base + 44 + m * 48;
        let mut v = [0f64; 6];
        for (k, x) in v.iter_mut().enumerate() { *x = f64::from_le_bytes(c.get(at + 8 * k..at + 8 * k + 8)?.try_into().ok()?); }
        matrices.push(v);
    }
    let q = &recs[pic_node.index].payload;
    let mut points = [0i32; 8];
    for (k, x) in points.iter_mut().enumerate() { *x = i32_at(q, 12 + 4 * k)?; }
    let crop = [i32_at(q, 44)?, i32_at(q, 48)?, i32_at(q, 52)?, i32_at(q, 56)?];
    let extra = q.get(73..).unwrap_or(&[]);
    let dims = if extra.len() == 17 || extra.len() == 18 { Some((u32_at(extra, 9)?, u32_at(extra, 13)?)) } else { None };
    Some(PictureRecord {
        attr: u32_at(&h.payload, 4)?, width: u32_at(&h.payload, 16)?, height: u32_at(&h.payload, 20)?,
        original: (u32_at(c, base + 12)?, u32_at(c, base + 16)?), current: (u32_at(c, base + 20)?, u32_at(c, base + 24)?),
        flip: u32_at(c, base + 28)?, rotation: i16::from_le_bytes([*c.get(base + 32)?, *c.get(base + 33)?]),
        rotation_center: (i32_at(c, base + 34)?, i32_at(c, base + 38)?), matrices,
        points, crop, bin_id: picture_bin_id(q)?, dims,
    })
}

impl Hwp5 {
    pub fn doc_info(&self) -> &[Record] { self.records.get("/DocInfo").map(|v| v.as_slice()).unwrap_or(&[]) }

    pub fn section_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.records.keys().filter(|k| k.starts_with("/BodyText/Section")).cloned().collect();
        v.sort_by_key(|k| k.trim_start_matches("/BodyText/Section").parse::<u32>().unwrap_or(u32::MAX));
        v
    }

    pub fn char_shapes(&self) -> Vec<CharShapeView> { self.doc_info().iter().filter(|r| r.tag == TAG_CHAR_SHAPE).filter_map(|r| parse_char_shape(&r.payload)).collect() }
    pub fn para_shapes(&self) -> Vec<ParaShapeView> { self.doc_info().iter().filter(|r| r.tag == TAG_PARA_SHAPE).filter_map(|r| parse_para_shape(&r.payload)).collect() }

    /// Face names grouped by the 7 language slots of ID_MAPPINGS.
    pub fn face_names(&self) -> Vec<Vec<String>> {
        let ids = self.id_mappings();
        let names: Vec<String> = self.doc_info().iter().filter(|r| r.tag == TAG_FACE_NAME).map(|r| parse_face_name(&r.payload).unwrap_or_default()).collect();
        let mut out = Vec::new();
        let mut at = 0usize;
        for lang in 0..7 {
            let n = ids.get(1 + lang).copied().unwrap_or(0).max(0) as usize;
            out.push(names.get(at..(at + n).min(names.len())).map(|s| s.to_vec()).unwrap_or_default());
            at += n;
        }
        out
    }

    pub fn id_mappings(&self) -> Vec<i32> {
        self.doc_info().iter().find(|r| r.tag == TAG_ID_MAPPINGS).map(|r| r.payload.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect()).unwrap_or_default()
    }

    pub fn declared_section_count(&self) -> Option<u16> {
        self.doc_info().iter().find(|r| r.tag == TAG_DOCUMENT_PROPERTIES).and_then(|r| r.payload.get(0..2).map(|b| u16::from_le_bytes([b[0], b[1]])))
    }

    /// DocInfo consistency: ID_MAPPINGS counts versus records.
    pub fn doc_info_consistency(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let ids = self.id_mappings();
        let count = |tag: u16| self.doc_info().iter().filter(|r| r.tag == tag).count() as i32;
        if ids.len() < 15 { problems.push(format!("ID_MAPPINGS has {} entries", ids.len())); return problems; }
        for (idx, tag, name) in [(0, TAG_BIN_DATA, "BIN_DATA"), (8, TAG_BORDER_FILL, "BORDER_FILL"), (9, TAG_CHAR_SHAPE, "CHAR_SHAPE"), (10, TAG_TAB_DEF, "TAB_DEF"), (11, TAG_NUMBERING, "NUMBERING"), (12, TAG_BULLET, "BULLET"), (13, TAG_PARA_SHAPE, "PARA_SHAPE"), (14, TAG_STYLE, "STYLE")] {
            if ids[idx] != count(tag) { problems.push(format!("ID_MAPPINGS[{idx}] {name}={} but {} records", ids[idx], count(tag))); }
        }
        let fonts: i32 = ids[1..8].iter().sum();
        if fonts != count(TAG_FACE_NAME) { problems.push(format!("ID_MAPPINGS fonts={fonts} but {} FACE_NAME", count(TAG_FACE_NAME))); }
        if let Some(n) = self.declared_section_count() {
            if n as usize != self.section_paths().len() { problems.push(format!("DOCUMENT_PROPERTIES sections={n} but {} BodyText streams", self.section_paths().len())); }
        }
        problems
    }

    /// BIN_DATA records in order; the 1-based position is what pictures reference.
    pub fn bin_data_entries(&self) -> Vec<Option<BinDataEntry>> {
        self.doc_info().iter().filter(|r| r.tag == TAG_BIN_DATA).map(|r| parse_bin_data(&r.payload)).collect()
    }

    /// Decoded bytes of the embedded BinData at 1-based `position`: the stream the record names,
    /// inflated when the record's compression flag (or the document default) says it is compressed.
    pub fn bin_data_payload(&self, position: u16) -> Result<Vec<u8>, String> {
        let entries = self.bin_data_entries();
        let entry = position.checked_sub(1).and_then(|i| entries.get(usize::from(i))).ok_or_else(|| format!("BinData position {position} does not exist ({} records)", entries.len()))?;
        let entry = entry.as_ref().ok_or_else(|| format!("BinData {position} is a link or malformed record"))?;
        if entry.kind() != 1 { return Err(format!("BinData {position} has type {} (embedding expected)", entry.kind())); }
        let path = entry.stream_path();
        let stream = self.streams.get(&path).ok_or_else(|| format!("stream {path} of BinData {position} is missing"))?;
        let compressed = match entry.compression() { 0 => self.header.compressed(), 1 => true, 2 => false, c => return Err(format!("BinData {position} has compression flag {c}")) };
        if compressed { inflate_raw(&stream.raw).map_err(|e| format!("{path}: {e}")) } else { Ok(stream.raw.clone()) }
    }

    /// The `control`-th CTRL_HEADER of top-level paragraph `para` in section `section`, parsed as a picture.
    pub fn picture_at(&self, section: usize, para: usize, control: usize) -> Result<PictureRecord, String> {
        let path = self.section_paths().get(section).cloned().ok_or_else(|| format!("section {section} does not exist"))?;
        let recs = &self.records[&path];
        let tree = build_tree(recs)?;
        let node = tree.get(para).ok_or_else(|| format!("{path}: paragraph {para} does not exist"))?;
        let ctrl = node.children.iter().filter(|c| recs[c.index].tag == TAG_CTRL_HEADER).nth(control).ok_or_else(|| format!("{path}: paragraph {para} has no control {control}"))?;
        picture_record(recs, ctrl).ok_or_else(|| format!("{path}: paragraph {para} control {control} is not a picture"))
    }

    pub fn ctrl_inventory(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for p in self.section_paths() {
            for r in &self.records[&p] {
                if r.tag == TAG_CTRL_HEADER && r.payload.len() >= 4 {
                    *m.entry(ctrl_id_string(u32::from_le_bytes(r.payload[0..4].try_into().unwrap()))).or_insert(0) += 1;
                }
            }
        }
        m
    }
}

fn list_end_flag(r: &Record) -> bool { r.payload.get(3).map(|b| b & 0x80 != 0).unwrap_or(false) }

/// The last paragraph of every paragraph list (section body, table cell) carries bit 31 of
/// nchars; no other paragraph may carry it.
pub fn list_end_problems(records: &[Record], tree: &[RecNode]) -> Vec<String> {
    let mut problems = Vec::new();
    let check_list = |paras: &[&RecNode], scope: &str, problems: &mut Vec<String>| {
        for (k, node) in paras.iter().enumerate() {
            let last = k + 1 == paras.len();
            if list_end_flag(&records[node.index]) != last {
                problems.push(format!("{scope}: paragraph {k} of {} has list-end flag {}", paras.len(), !last));
            }
        }
    };
    let top: Vec<&RecNode> = tree.iter().filter(|n| records[n.index].tag == TAG_PARA_HEADER).collect();
    check_list(&top, "section body", &mut problems);
    fn walk(records: &[Record], node: &RecNode, problems: &mut Vec<String>, check: &dyn Fn(&[&RecNode], &str, &mut Vec<String>)) {
        for child in &node.children {
            if records[child.index].tag == TAG_CTRL_HEADER {
                let id = records[child.index].payload.get(0..4).map(|b| ctrl_id_string(u32::from_le_bytes(b.try_into().unwrap()))).unwrap_or_default();
                if id == "tbl " {
                    let mut cell: Vec<&RecNode> = Vec::new();
                    let mut started = false;
                    for g in &child.children {
                        match records[g.index].tag {
                            TAG_LIST_HEADER => { if started { check(&cell, "table cell", problems); } cell.clear(); started = true; }
                            TAG_PARA_HEADER if started => cell.push(g),
                            _ => {}
                        }
                    }
                    if started { check(&cell, "table cell", problems); }
                }
                for g in &child.children { if records[g.index].tag == TAG_PARA_HEADER { walk(records, g, problems, check); } }
            }
        }
    }
    for node in &top { walk(records, node, &mut problems, &check_list); }
    problems
}

/// Paragraph-level structural problems in one section (empty = consistent).
pub fn section_consistency(records: &[Record]) -> Vec<String> {
    let mut problems = Vec::new();
    match build_tree(records) {
        Ok(tree) => problems.extend(list_end_problems(records, &tree)),
        Err(e) => problems.push(e),
    }
    let mut current: Option<(usize, ParaHeader, u16)> = None;
    let mut text_units = 0usize;
    let mut char_runs: Vec<(u32, u32)> = Vec::new();
    let mut line_bytes = 0usize;
    let flush = |cur: &Option<(usize, ParaHeader, u16)>, text_units: usize, char_runs: &Vec<(u32, u32)>, line_bytes: usize, problems: &mut Vec<String>| {
        if let Some((i, h, _)) = cur {
            if text_units != 0 && text_units as u32 != h.nchars { problems.push(format!("paragraph record {i}: nchars {} != PARA_TEXT units {text_units}", h.nchars)); }
            if char_runs.len() != h.char_shape_count as usize { problems.push(format!("paragraph record {i}: declared {} char shape runs, found {}", h.char_shape_count, char_runs.len())); }
            if line_bytes != h.line_seg_count as usize * 36 { problems.push(format!("paragraph record {i}: declared {} line segs, PARA_LINE_SEG has {line_bytes} bytes", h.line_seg_count)); }
            if char_runs.first().map(|r| r.0 != 0).unwrap_or(false) { problems.push(format!("paragraph record {i}: first char shape run does not start at 0")); }
            if char_runs.windows(2).any(|w| w[0].0 >= w[1].0) { problems.push(format!("paragraph record {i}: char shape positions are not increasing")); }
        }
    };
    for (i, r) in records.iter().enumerate() {
        if r.tag == TAG_PARA_HEADER {
            flush(&current, text_units, &char_runs, line_bytes, &mut problems);
            current = para_header(&r.payload).map(|h| (i, h, r.level));
            if current.is_none() { problems.push(format!("paragraph record {i}: short PARA_HEADER")); }
            text_units = 0; char_runs.clear(); line_bytes = 0;
            continue;
        }
        let Some((_, _, level)) = current else { continue };
        if r.level != level + 1 { continue; }
        match r.tag {
            TAG_PARA_TEXT => text_units = r.payload.len() / 2,
            TAG_PARA_CHAR_SHAPE => char_runs = r.payload.chunks_exact(8).map(|c| (u32::from_le_bytes(c[0..4].try_into().unwrap()), u32::from_le_bytes(c[4..8].try_into().unwrap()))).collect(),
            TAG_PARA_LINE_SEG => line_bytes = r.payload.len(),
            _ => {}
        }
    }
    flush(&current, text_units, &char_runs, line_bytes, &mut problems);
    problems
}
