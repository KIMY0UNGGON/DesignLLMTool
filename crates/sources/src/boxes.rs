//! Page boxes, rotation, UserUnit, and printed page labels.
//! MediaBox, CropBox, Rotate, and Resources are inheritable from /Pages nodes (PDF 1.7 section 7.7.3.4).
//! UserUnit is a page-object entry and is not inherited.

use crate::content::{Meter, Unit};
use crate::error::{PResult, PdfErr};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeSet;

/// Normalized rectangle in default user space (PDF points before UserUnit scaling).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect4 {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect4 {
    pub(crate) fn from_array(a: [f64; 4]) -> Self {
        Self { x0: a[0].min(a[2]), y0: a[1].min(a[3]), x1: a[0].max(a[2]), y1: a[1].max(a[3]) }
    }

    pub(crate) fn intersect(self, other: Rect4) -> Option<Rect4> {
        let out = Rect4 {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        };
        if out.x1 > out.x0 && out.y1 > out.y0 { Some(out) } else { None }
    }

    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PageBoxes {
    pub media: Rect4,
    /// CropBox intersected with MediaBox (the visible area).
    pub crop: Rect4,
    /// Normalized to 0, 90, 180, or 270.
    pub rotate: u16,
    pub user_unit: f64,
}

/// Dictionaries read while climbing /Parent for one key: the page itself, then every Pages node above it, up to the root.
/// A page under a Pages node at depth d (root = 0) needs d + 2 reads, so the limit must cover the deepest accepted node
/// (`MAX_PAGE_TREE_DEPTH` in lib.rs) plus those two reads. The assertion keeps that true when either value changes.
const MAX_TREE_DEPTH: u32 = 66;
const _: () = assert!(crate::MAX_PAGE_TREE_DEPTH + 2 <= MAX_TREE_DEPTH as usize);

/// Most references that are followed in one chain. A longer chain, or one that loops, is an error. The value is lopdf's own limit
/// for `Document::get_object`, so both paths accept the same chains.
pub(crate) const MAX_REFERENCE_HOPS: usize = 128;

/// Follows references until a direct object is reached, so a reference to a reference resolves to the object that the chain names.
pub(crate) fn resolve<'a>(doc: &'a Document, object: &'a Object) -> PResult<&'a Object> {
    let mut current = object;
    for _ in 0..MAX_REFERENCE_HOPS {
        match current {
            Object::Reference(id) => current = doc.get_object(*id)?,
            direct => return Ok(direct),
        }
    }
    Err(PdfErr::Parse("a reference chain is too long or loops".into()))
}

/// The object number of the indirect object that holds the value a reference names, after its chain of references is followed.
/// None for a direct object, which has no number. Two references that name one object give the same number.
///
/// This walks the stored objects rather than calling `Document::get_object`, which follows the chain itself and returns the
/// value without its number.
pub(crate) fn final_id(doc: &Document, object: &Object) -> PResult<Option<ObjectId>> {
    let Object::Reference(first) = object else {
        return Ok(None);
    };
    let mut id = *first;
    for _ in 0..MAX_REFERENCE_HOPS {
        match doc.objects.get(&id) {
            Some(Object::Reference(next)) => id = *next,
            Some(_) => return Ok(Some(id)),
            None => return Err(PdfErr::Parse("a reference names an object that the document does not have".into())),
        }
    }
    Err(PdfErr::Parse("a reference chain is too long or loops".into()))
}

pub(crate) fn number(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

/// Looks up an inheritable key from the page dictionary up through /Parent.
pub(crate) fn inherited<'a>(doc: &'a Document, page_id: ObjectId, key: &[u8]) -> PResult<Option<&'a Object>> {
    let mut current = page_id;
    for _ in 0..MAX_TREE_DEPTH {
        let dict = doc.get_dictionary(current)?;
        if let Ok(value) = dict.get(key) {
            return Ok(Some(resolve(doc, value)?));
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(parent)) => current = *parent,
            _ => return Ok(None),
        }
    }
    Err(PdfErr::Parse("page tree is too deep or cyclic".into()))
}

fn box_from(doc: &Document, object: &Object) -> PResult<Option<[f64; 4]>> {
    let array = resolve(doc, object)?.as_array()?;
    if array.len() < 4 {
        return Ok(None);
    }
    let mut out = [0.0f64; 4];
    for (slot, item) in out.iter_mut().zip(array.iter()) {
        match number(resolve(doc, item)?) {
            Some(value) => *slot = value,
            None => return Ok(None),
        }
    }
    if out.iter().any(|v| !v.is_finite()) {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Reads boxes, rotation, and UserUnit for one physical page (0-based `index` is used only in messages).
pub(crate) fn page_boxes(doc: &Document, page_id: ObjectId, index: u32) -> PResult<PageBoxes> {
    let media_object = inherited(doc, page_id, b"MediaBox")?.ok_or_else(|| {
        PdfErr::unsupported("PDF_MISSING_MEDIABOX", format!("physical page {index} has no MediaBox"))
    })?;
    let media_raw = box_from(doc, media_object)?.ok_or_else(|| {
        PdfErr::invalid("PDF_MISSING_MEDIABOX", format!("physical page {index} has an invalid MediaBox"))
    })?;
    let media = Rect4::from_array(media_raw);
    if media.x1 <= media.x0 || media.y1 <= media.y0 {
        return Err(PdfErr::invalid("PDF_MISSING_MEDIABOX", format!("physical page {index} has an empty MediaBox")));
    }

    let crop = match inherited(doc, page_id, b"CropBox")? {
        Some(object) => match box_from(doc, object)? {
            Some(raw) => Rect4::from_array(raw).intersect(media).unwrap_or(media),
            None => media,
        },
        None => media,
    };

    let rotate_raw = match inherited(doc, page_id, b"Rotate")? {
        Some(object) => number(object).ok_or_else(|| {
            PdfErr::invalid("PDF_BAD_ROTATE", format!("physical page {index} has a /Rotate that is not a number"))
        })?,
        None => 0.0,
    };
    if rotate_raw.fract() != 0.0 || (rotate_raw as i64).rem_euclid(90) != 0 {
        return Err(PdfErr::invalid(
            "PDF_BAD_ROTATE",
            format!("physical page {index} has /Rotate {rotate_raw}, which is not a multiple of 90"),
        ));
    }
    let rotate = (rotate_raw as i64).rem_euclid(360) as u16;

    // UserUnit is read from the page object only. Absent means the default of 1.
    let user_unit = {
        let page = doc.get_dictionary(page_id)?;
        match page.get(b"UserUnit") {
            Ok(value) => match number(resolve(doc, value)?) {
                Some(v) => v,
                None => {
                    return Err(PdfErr::invalid(
                        "PDF_BAD_USERUNIT",
                        format!("physical page {index} has a non-numeric /UserUnit"),
                    ))
                }
            },
            Err(_) => 1.0,
        }
    };
    crate::units::Units::new(user_unit)?;

    Ok(PageBoxes { media, crop, rotate, user_unit })
}

/// Longest /P prefix that is kept, in bytes. A longer prefix would be copied into every evidence item of its pages.
pub(crate) const MAX_PREFIX_BYTES: usize = 256;
/// Longest printed label that is kept, in bytes, prefix included. An alphabetic label grows with its page number.
pub(crate) const MAX_LABEL_BYTES: usize = 512;
/// Most /Nums entries read from one document. A larger tree gives the document no printed labels.
pub(crate) const MAX_LABEL_ENTRIES: usize = 100_000;

/// The numbering style of a /PageLabels range. The style name is compared with the known names and is never copied, so a name
/// of any length gives one of these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LabelStyle {
    Decimal,
    UpperRoman,
    LowerRoman,
    UpperAlpha,
    LowerAlpha,
    /// No style, or a name that is not one of the five. The label is the prefix alone.
    Unnumbered,
}

impl LabelStyle {
    fn from_name(name: &[u8]) -> LabelStyle {
        match name {
            b"D" => LabelStyle::Decimal,
            b"R" => LabelStyle::UpperRoman,
            b"r" => LabelStyle::LowerRoman,
            b"A" => LabelStyle::UpperAlpha,
            b"a" => LabelStyle::LowerAlpha,
            _ => LabelStyle::Unnumbered,
        }
    }
}

/// One /Nums range of /PageLabels.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabelRange {
    pub key: i64,
    pub style: LabelStyle,
    /// The /P prefix, or None when it is longer than `MAX_PREFIX_BYTES`. The pages of such a range get their label omitted.
    pub prefix: Option<String>,
    pub start: i64,
}

/// Reads the catalog's /PageLabels number tree. Both /Nums leaves and /Kids nodes are supported. Each node and each entry is one
/// unit of work on the request, and a node is read once however many references name it.
pub(crate) fn read_labels(doc: &Document, request: &Meter) -> PResult<Vec<LabelRange>> {
    let root = doc.catalog()?;
    let Ok(labels) = root.get(b"PageLabels") else {
        return Ok(Vec::new());
    };
    let tree = resolve(doc, labels)?.as_dict()?;
    let mut visited = BTreeSet::new();
    if let Some(id) = final_id(doc, labels)? {
        visited.insert(id);
    }
    let mut out = Vec::new();
    collect_nums(doc, tree, &mut out, 0, &mut visited, request)?;
    out.sort_by_key(|r| r.key);
    Ok(out)
}

/// The value of `key` in `dict`, with a reference followed. A missing key, or a reference that does not resolve, is None.
fn value_of<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    resolve(doc, dict.get(key).ok()?).ok()
}

/// Adds the /Nums entries of `node` and of the nodes below it. A node that is named twice is an error: each node is read once,
/// by the object number it lives under, so a tree whose kids share nodes, directly or through aliases, cannot make the reader
/// visit them once per path.
fn collect_nums(
    doc: &Document,
    node: &Dictionary,
    out: &mut Vec<LabelRange>,
    depth: u32,
    visited: &mut BTreeSet<ObjectId>,
    request: &Meter,
) -> PResult<()> {
    if depth > 32 {
        return Err(PdfErr::Parse("page label tree is too deep".into()));
    }
    request.charge(Unit::Work, 1)?;
    if let Ok(nums) = node.get(b"Nums") {
        let array = resolve(doc, nums)?.as_array()?;
        for pair in array.chunks(2) {
            if pair.len() != 2 {
                return Err(PdfErr::Parse("odd number of entries in /Nums".into()));
            }
            if out.len() >= MAX_LABEL_ENTRIES {
                return Err(PdfErr::Parse("page label tree has too many entries".into()));
            }
            request.charge(Unit::Work, 1)?;
            let key = resolve(doc, &pair[0])?.as_i64()?;
            let entry = resolve(doc, &pair[1])?.as_dict()?;
            let style = match value_of(doc, entry, b"S") {
                Some(Object::Name(name)) => LabelStyle::from_name(name),
                _ => LabelStyle::Unnumbered,
            };
            let prefix = match value_of(doc, entry, b"P") {
                Some(Object::String(bytes, _)) if bytes.len() > MAX_PREFIX_BYTES => None,
                Some(Object::String(bytes, _)) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => Some(String::new()),
            };
            let start = value_of(doc, entry, b"St").and_then(|o| o.as_i64().ok()).unwrap_or(1);
            out.push(LabelRange { key, style, prefix, start });
        }
    }
    if let Ok(kids) = node.get(b"Kids") {
        let array = resolve(doc, kids)?.as_array()?;
        for kid in array {
            if let Some(id) = final_id(doc, kid)? {
                if !visited.insert(id) {
                    return Err(PdfErr::Parse("a page label tree node is listed more than once".into()));
                }
            }
            let child = resolve(doc, kid)?.as_dict()?;
            collect_nums(doc, child, out, depth + 1, visited, request)?;
        }
    }
    Ok(())
}

/// Largest label number that is written out. A larger number, or one that overflows, gives the page no label.
const MAX_LABEL_NUMBER: i64 = 100_000;

/// The printed label of one page.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LabelState {
    /// The document declares no label for this page, or its number is not representable.
    None,
    Label(String),
    /// The document declares a label for this page, but it is longer than the supported length, so it is omitted.
    Omitted,
}

/// Printed label of the 0-based physical index, from the nearest range whose key is at or before it. The label is never used
/// as an index. `read_labels` sorts the ranges by key, so a binary search finds the range.
pub(crate) fn label_state(ranges: &[LabelRange], index: u32) -> LabelState {
    let idx = i64::from(index);
    let at = ranges.partition_point(|r| r.key <= idx);
    let Some(range) = at.checked_sub(1).and_then(|i| ranges.get(i)) else {
        return LabelState::None;
    };
    let Some(offset) = idx.checked_sub(range.key) else {
        return LabelState::None;
    };
    let Some(n) = range.start.checked_add(offset) else {
        return LabelState::None;
    };
    if n > MAX_LABEL_NUMBER {
        return LabelState::None;
    }
    let Some(prefix) = &range.prefix else {
        return LabelState::Omitted;
    };
    let body = match range.style {
        LabelStyle::Decimal => n.to_string(),
        LabelStyle::UpperRoman => roman(n, true),
        LabelStyle::LowerRoman => roman(n, false),
        LabelStyle::UpperAlpha => alpha(n, true),
        LabelStyle::LowerAlpha => alpha(n, false),
        LabelStyle::Unnumbered => String::new(),
    };
    let label = format!("{prefix}{body}");
    if label.len() > MAX_LABEL_BYTES {
        LabelState::Omitted
    } else {
        LabelState::Label(label)
    }
}

fn roman(mut n: i64, upper: bool) -> String {
    if n < 1 {
        return String::new();
    }
    const TABLE: [(i64, &str); 13] = [
        (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"), (50, "l"),
        (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i"),
    ];
    let mut s = String::new();
    for (value, text) in TABLE.iter() {
        while n >= *value {
            s.push_str(text);
            n -= value;
        }
    }
    if upper { s.to_uppercase() } else { s }
}

fn alpha(n: i64, upper: bool) -> String {
    if n < 1 {
        return String::new();
    }
    let letter = (b'a' + ((n - 1) % 26) as u8) as char;
    let reps = ((n - 1) / 26 + 1) as usize;
    let s: String = std::iter::repeat_n(letter, reps).collect();
    if upper { s.to_uppercase() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::REQUEST_BUDGET;

    #[test]
    fn roman_and_alpha_labels() {
        assert_eq!(roman(1, false), "i");
        assert_eq!(roman(4, false), "iv");
        assert_eq!(roman(14, true), "XIV");
        assert_eq!(alpha(1, false), "a");
        assert_eq!(alpha(27, true), "AA");
    }

    fn label(state: LabelState) -> Option<String> {
        match state {
            LabelState::Label(text) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn label_uses_largest_key_at_or_before_index() {
        let ranges = vec![
            LabelRange { key: 0, style: LabelStyle::LowerRoman, prefix: Some(String::new()), start: 1 },
            LabelRange { key: 2, style: LabelStyle::Decimal, prefix: Some(String::new()), start: 1 },
        ];
        assert_eq!(label(label_state(&ranges, 0)).as_deref(), Some("i"));
        assert_eq!(label(label_state(&ranges, 1)).as_deref(), Some("ii"));
        assert_eq!(label(label_state(&ranges, 2)).as_deref(), Some("1"));
        assert_eq!(label(label_state(&ranges, 11)).as_deref(), Some("10"));
        assert_eq!(label_state(&[], 3), LabelState::None);
    }

    #[test]
    fn a_prefix_of_256_bytes_is_kept_and_a_longer_one_is_omitted() {
        let kept = vec![LabelRange { key: 0, style: LabelStyle::Unnumbered, prefix: Some("P".repeat(MAX_PREFIX_BYTES)), start: 1 }];
        assert_eq!(label(label_state(&kept, 0)).map(|s| s.len()), Some(MAX_PREFIX_BYTES));
        // A range whose prefix was longer than the limit has no prefix kept, so its pages are omitted.
        let omitted = vec![LabelRange { key: 0, style: LabelStyle::Unnumbered, prefix: None, start: 1 }];
        assert_eq!(label_state(&omitted, 0), LabelState::Omitted);
    }

    #[test]
    fn an_alphabetic_label_longer_than_the_label_limit_is_omitted() {
        let ranges = vec![LabelRange { key: 0, style: LabelStyle::UpperAlpha, prefix: Some(String::new()), start: 1 }];
        // Number 26 * 512 + 1 is written as 513 letters, which is longer than MAX_LABEL_BYTES.
        assert_eq!(label_state(&ranges, 26 * 512), LabelState::Omitted);
        assert!(label(label_state(&ranges, 25)).is_some());
    }

    #[test]
    fn a_style_name_is_compared_and_any_other_name_is_unnumbered() {
        assert_eq!(LabelStyle::from_name(b"D"), LabelStyle::Decimal);
        assert_eq!(LabelStyle::from_name(b"a"), LabelStyle::LowerAlpha);
        assert_eq!(LabelStyle::from_name(&vec![b'N'; 1 << 20]), LabelStyle::Unnumbered);
        assert_eq!(LabelStyle::from_name(b"d"), LabelStyle::Unnumbered);
    }

    #[test]
    fn rect_intersection_and_containment() {
        let media = Rect4 { x0: 0.0, y0: 0.0, x1: 612.0, y1: 792.0 };
        let crop = Rect4 { x0: 18.0, y0: 20.0, x1: 594.0, y1: 760.0 };
        assert_eq!(crop.intersect(media), Some(crop));
        assert!(crop.contains(18.0, 20.0));
        assert!(!crop.contains(17.9, 20.0));
        let outside = Rect4 { x0: 700.0, y0: 0.0, x1: 800.0, y1: 10.0 };
        assert_eq!(outside.intersect(media), None);
    }

    #[test]
    fn references_are_followed_to_the_object_they_name_and_a_loop_is_refused() {
        let mut doc = Document::new();
        doc.objects.insert((1, 0), Object::Reference((2, 0)));
        doc.objects.insert((2, 0), Object::Reference((3, 0)));
        doc.objects.insert((3, 0), Object::Integer(5));
        doc.objects.insert((4, 0), Object::Reference((5, 0)));
        doc.objects.insert((5, 0), Object::Reference((4, 0)));
        assert_eq!(resolve(&doc, &Object::Reference((1, 0))).unwrap(), &Object::Integer(5));
        assert_eq!(final_id(&doc, &Object::Reference((1, 0))).unwrap(), Some((3, 0)));
        assert_eq!(final_id(&doc, &Object::Integer(1)).unwrap(), None);
        assert!(resolve(&doc, &Object::Reference((4, 0))).is_err());
        assert!(final_id(&doc, &Object::Reference((4, 0))).is_err());
    }

    #[test]
    fn a_label_tree_read_charges_the_request_and_stops_at_its_limit() {
        // Catalog 1 names the label tree 2, which has one range.
        let mut labels = Dictionary::new();
        labels.set("Nums", Object::Array(vec![Object::Integer(0), Object::Dictionary(Dictionary::new())]));
        let mut catalog = Dictionary::new();
        catalog.set("PageLabels", Object::Reference((2, 0)));
        let mut doc = Document::new();
        doc.objects.insert((1, 0), Object::Dictionary(catalog));
        doc.objects.insert((2, 0), Object::Dictionary(labels));
        doc.trailer.set("Root", Object::Reference((1, 0)));
        let meter = Meter::request(REQUEST_BUDGET);
        let ranges = read_labels(&doc, &meter).expect("a small tree is read");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].style, LabelStyle::Unnumbered);
        let tight = Meter::request(crate::content::Budget { work: 1, ..REQUEST_BUDGET });
        assert_eq!(read_labels(&doc, &tight).unwrap_err().code(), "PDF_REQUEST_BUDGET");
    }
}
