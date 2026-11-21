//! Content stream interpreter for text. Port of G0 candidate A (lopdf parser + own text state machine).
//!
//! Supported operators: q Q cm BT ET Tc Tw Tz TL Ts Tr Tf Td TD Tm T* Tj TJ ' " Do (XObjects: see below), and the marked-content
//! operators BDC BMC EMC. Text in an optional content group that the document's default configuration sets OFF is left out of the
//! evidence, and a group or membership that this reader does not interpret fails its page.
//! Coordinates are default user space points (after CTM, before UserUnit and crop/rotation).
//! Not supported (explicit errors or stated limits): Type3 fonts, vertical writing, path clipping (W n),
//! and text rotated relative to the baseline (each glyph becomes its own line in evidence grouping).
//! A content stream that does not parse to its end fails its page. A shorter operator list is never used as the page.
//! An inline image whose data end is not proven (any filtered image, a dictionary length that does not end at `EI`, or a
//! CR LF after `ID` that gives no single data end) fails its page too. Its bytes are never parsed as operators.
//! An operator with the wrong number or type of operands fails its page.
//! Each XObject that `Do` names is handled by its /Subtype. A Form is run. An Image is skipped and counted: a page with text
//! reports the count as PDF_IMAGE_NOT_READ, and a page without text reports it as PDF_PAGE_NO_TEXT.
//! A PostScript XObject (/Subtype /PS, ISO 32000-1 8.8.2) is not executed by PDF readers and draws no PDF text: it is skipped,
//! and the page is still read. Each use is counted in the non-blocking diagnostic PDF_XOBJECT_POSTSCRIPT.
//! A null entry, or a reference that names no object, is the same as a missing name: nothing is drawn, and each use is
//! counted in PDF_XOBJECT_MISSING. An XObject of any other subtype, or with none, fails its page (PDF_XOBJECT_UNKNOWN).

use crate::boxes::{final_id, resolve};
use crate::decode::decode_complete;
use crate::error::{PResult, PdfErr};
use crate::font::Font;
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

const MAX_FORM_DEPTH: u32 = 16;
/// Nesting of q operators that one content stream may keep. The PDF reference gives 28 as the nesting limit of graphics states, so a
/// deeper page is refused rather than read.
const MAX_GRAPHICS_STATE_DEPTH: usize = 28;
/// Nesting of marked-content sequences (BMC, BDC) that one content stream may keep. The reference sets no limit, so this bounds the
/// stack of open sequences.
const MAX_MARKED_CONTENT_DEPTH: usize = 128;
/// Most content streams that one page may list in /Contents. A page lists a few. Each entry is also charged as work.
const MAX_CONTENT_STREAMS: usize = 4096;
/// Wall-clock time that one request may take. Every charge checks it, so it also bounds work that the units do not count.
const REQUEST_TIME_LIMIT: Duration = Duration::from_secs(60);
const MIB: u64 = 1 << 20;

/// The kinds of work that the limits count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    /// Bytes produced by decoding a stream: content, forms, fonts and their CMaps.
    Decoded,
    /// Tokens of a content stream, counted before the stream is parsed. Each token yields at most one object or operator.
    Tokens,
    /// Operators run, TJ items shown, resource entries read, annotation entries read, and label nodes read.
    Work,
    /// Glyphs collected for evidence.
    Glyphs,
    /// Form XObject runs. A form counts once per `Do` that runs it.
    FormRuns,
    /// Entries of font mappings and width tables that are loaded.
    Entries,
    /// Evidence items made from the text lines of a page. Each item is kept for its request.
    Evidence,
}

/// Limits of one scope: a page, or a request, which is all of its pages together with its label tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Budget {
    pub decoded: u64,
    pub tokens: u64,
    pub work: u64,
    pub glyphs: u64,
    pub form_runs: u64,
    pub entries: u64,
    pub evidence: u64,
}

impl Budget {
    fn limit(&self, unit: Unit) -> u64 {
        match unit {
            Unit::Decoded => self.decoded,
            Unit::Tokens => self.tokens,
            Unit::Work => self.work,
            Unit::Glyphs => self.glyphs,
            Unit::FormRuns => self.form_runs,
            Unit::Entries => self.entries,
            Unit::Evidence => self.evidence,
        }
    }
}

/// Limits of one page. Generous for real pages: a dense page has thousands of glyphs and operators. The token limit is the one that
/// bounds memory: each token becomes at most one parsed object or operator, and each of those takes tens of bytes. The evidence limit
/// is the number of text lines that a page may turn into evidence items.
pub(crate) const PAGE_BUDGET: Budget = Budget {
    decoded: 64 * MIB,
    tokens: 1_000_000,
    work: 5_000_000,
    glyphs: 1_000_000,
    form_runs: 100_000,
    entries: 1_000_000,
    evidence: 20_000,
};

/// Limits of one request. They bound the total work of a request, which the page limits do not: a request of 4096 pages, each at
/// its page limit, would do 4096 times that work.
pub(crate) const REQUEST_BUDGET: Budget = Budget {
    decoded: 1024 * MIB,
    tokens: 20_000_000,
    work: 100_000_000,
    glyphs: 10_000_000,
    form_runs: 1_000_000,
    entries: 4_000_000,
    evidence: 200_000,
};

/// Which limit an exhausted meter reports: the page's, or the request's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Page,
    Request,
}

/// Units counted for one scope against its limits. A count never passes its limit: `charge` refuses first and counts nothing.
#[derive(Debug)]
pub(crate) struct Meter {
    budget: Budget,
    level: Level,
    used: [Cell<u64>; 7],
    /// When the request must have finished. Only a request's meter has one.
    deadline: Option<Instant>,
}

impl Meter {
    pub(crate) fn page(budget: Budget) -> Meter {
        Meter::new(budget, Level::Page, None)
    }

    pub(crate) fn request(budget: Budget) -> Meter {
        Meter::request_until(budget, Instant::now() + REQUEST_TIME_LIMIT)
    }

    pub(crate) fn request_until(budget: Budget, deadline: Instant) -> Meter {
        Meter::new(budget, Level::Request, Some(deadline))
    }

    fn new(budget: Budget, level: Level, deadline: Option<Instant>) -> Meter {
        Meter { budget, level, used: std::array::from_fn(|_| Cell::new(0)), deadline }
    }

    /// Counts `n` units of `unit`, or fails when the total would pass the limit. A request whose time has run out fails here too,
    /// so every charge point checks the deadline.
    pub(crate) fn charge(&self, unit: Unit, n: u64) -> PResult<()> {
        if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(PdfErr::unsupported("PDF_REQUEST_BUDGET", "the request runs longer than the supported time"));
        }
        let cell = &self.used[unit as usize];
        let total = cell.get().saturating_add(n);
        if total > self.budget.limit(unit) {
            return Err(self.exhausted(unit));
        }
        cell.set(total);
        Ok(())
    }

    /// Units of `unit` that may still be counted.
    pub(crate) fn room(&self, unit: Unit) -> u64 {
        self.budget.limit(unit).saturating_sub(self.used[unit as usize].get())
    }

    /// The error for a limit that `unit` has reached.
    pub(crate) fn exhausted(&self, unit: Unit) -> PdfErr {
        match (self.level, unit) {
            (Level::Request, _) => PdfErr::unsupported(
                "PDF_REQUEST_BUDGET",
                "the document has more work than the supported count for one request",
            ),
            (Level::Page, Unit::Decoded) => PdfErr::unsupported(
                "PDF_DECODE_BUDGET",
                "the page's content and forms decode to more bytes than the supported count for one page",
            ),
            (Level::Page, Unit::FormRuns) => PdfErr::unsupported(
                "PDF_FORM_BUDGET",
                "the page runs more form XObjects than the supported count for one page",
            ),
            (Level::Page, Unit::Tokens) => PdfErr::unsupported(
                "PDF_CONTENT_BUDGET",
                "the page's content has more tokens than the supported count for one page",
            ),
            (Level::Page, Unit::Glyphs) => PdfErr::unsupported(
                "PDF_CONTENT_BUDGET",
                "the page has more glyphs than the supported count for one page",
            ),
            (Level::Page, Unit::Work) => PdfErr::unsupported(
                "PDF_CONTENT_BUDGET",
                "the page runs more operators and items than the supported count for one page",
            ),
            (Level::Page, Unit::Entries) => PdfErr::unsupported(
                "PDF_CONTENT_BUDGET",
                "the page loads more font and width entries than the supported count for one page",
            ),
            (Level::Page, Unit::Evidence) => PdfErr::unsupported(
                "PDF_CONTENT_BUDGET",
                "the page has more text lines than the supported count of evidence items for one page",
            ),
        }
    }
}

/// The page meter and the request meter. Every unit is charged to both, so a page cannot pass its limit and a request cannot pass
/// its own.
#[derive(Clone, Copy)]
pub(crate) struct Meters<'a> {
    pub(crate) page: &'a Meter,
    pub(crate) request: &'a Meter,
}

impl Meters<'_> {
    pub(crate) fn charge(&self, unit: Unit, n: u64) -> PResult<()> {
        self.page.charge(unit, n)?;
        self.request.charge(unit, n)
    }

    /// Units of `unit` that a stage may still produce: the smaller room of the two meters.
    pub(crate) fn room(&self, unit: Unit) -> u64 {
        self.page.room(unit).min(self.request.room(unit))
    }

    /// The error for a stage that stopped at `room(unit)`: that of the meter with less room.
    pub(crate) fn exhausted(&self, unit: Unit) -> PdfErr {
        if self.request.room(unit) < self.page.room(unit) {
            self.request.exhausted(unit)
        } else {
            self.page.exhausted(unit)
        }
    }
}

/// What the pages of one request share: the request's meter, and the fonts loaded so far by the object number of each font.
pub(crate) struct Request {
    pub(crate) meter: Meter,
    fonts: RefCell<HashMap<ObjectId, Rc<Font>>>,
    /// The document's default optional content configuration. It is read once, by the first page that uses an optional content entry.
    optional: RefCell<Option<Rc<OptionalContent>>>,
}

impl Request {
    pub(crate) fn new() -> Request {
        Request::with_budget(REQUEST_BUDGET)
    }

    pub(crate) fn with_budget(budget: Budget) -> Request {
        Request { meter: Meter::request(budget), fonts: RefCell::new(HashMap::new()), optional: RefCell::new(None) }
    }

    /// A request that must finish by `deadline`. Production reads take `new`, which allows `REQUEST_TIME_LIMIT`. Only the tests
    /// give a deadline of their own.
    #[cfg(test)]
    pub(crate) fn with_limits(budget: Budget, deadline: Instant) -> Request {
        Request { meter: Meter::request_until(budget, deadline), fonts: RefCell::new(HashMap::new()), optional: RefCell::new(None) }
    }

    /// Fonts loaded for this request so far. The font cache tests read it.
    #[cfg(test)]
    pub(crate) fn loaded_fonts(&self) -> usize {
        self.fonts.borrow().len()
    }
}

/// PDF matrix `[a b c d e f]`, row-vector convention: point p maps to `p x M`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Matrix(pub [f64; 6]);

impl Matrix {
    pub(crate) const IDENTITY: Matrix = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub(crate) fn translate(tx: f64, ty: f64) -> Matrix {
        Matrix([1.0, 0.0, 0.0, 1.0, tx, ty])
    }

    /// `self x other`: `self` is applied first, then `other`.
    pub(crate) fn mul(self, other: Matrix) -> Matrix {
        let a = self.0;
        let b = other.0;
        Matrix([
            a[0] * b[0] + a[1] * b[2],
            a[0] * b[1] + a[1] * b[3],
            a[2] * b[0] + a[3] * b[2],
            a[2] * b[1] + a[3] * b[3],
            a[4] * b[0] + a[5] * b[2] + b[4],
            a[4] * b[1] + a[5] * b[3] + b[5],
        ])
    }

    pub(crate) fn apply(self, x: f64, y: f64) -> (f64, f64) {
        let m = self.0;
        (x * m[0] + y * m[2] + m[4], x * m[1] + y * m[3] + m[5])
    }

    pub(crate) fn det(self) -> f64 {
        self.0[0] * self.0[3] - self.0[1] * self.0[2]
    }
}

/// One shown character code with its geometry in default user space.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawGlyph {
    pub text: String,
    /// Glyph origin (text baseline start, including Rise).
    pub origin_x: f64,
    pub origin_y: f64,
    /// Axis-aligned glyph cell in user space: x0, y0, x1, y1. Uses font ascent and descent.
    pub cell: [f64; 4],
    /// Effective font size in user units (Tf size scaled by the text and CTM matrices).
    pub font_size: f64,
    pub render_mode: u8,
    /// Origin lies outside a Form XObject /BBox on the current form stack.
    pub clipped: bool,
    pub width_known: bool,
}

/// The font that the text state holds. `Tf` looks its name up in the resources in scope at that point, so the font is fixed by the
/// `Tf`, and a form that binds the same name to another font does not change it.
#[derive(Debug, Clone, Copy)]
enum FontSlot<'a> {
    /// No `Tf` has set a font yet.
    Unset,
    /// `Tf` named a resource that is not a font dictionary (absent, null, dangling, or of another type). Showing text then
    /// fails the page.
    Missing,
    Found(FontRef<'a>),
}

/// The graphics and text state. It holds no heap data, so saving it with `q` is a copy.
#[derive(Debug, Clone, Copy)]
struct GState<'a> {
    ctm: Matrix,
    tc: f64,
    tw: f64,
    th: f64,
    tl: f64,
    tfs: f64,
    tr: u8,
    rise: f64,
    font: FontSlot<'a>,
}

impl<'a> GState<'a> {
    fn initial() -> Self {
        GState { ctm: Matrix::IDENTITY, tc: 0.0, tw: 0.0, th: 1.0, tl: 0.0, tfs: 0.0, tr: 0, rise: 0.0, font: FontSlot::Unset }
    }
}

/// Inherited /Resources of the page (direct or referenced). A null entry is the same as a missing one, so the search goes on up
/// the /Parent chain. The page tree is checked before any page is read, and the search is bounded like the tree.
fn page_resources(doc: &Document, page_id: ObjectId) -> PResult<Option<&Dictionary>> {
    let mut current = page_id;
    for _ in 0..crate::MAX_PAGE_TREE_DEPTH + 2 {
        let dict = doc.get_dictionary(current)?;
        if let Ok(value) = dict.get(b"Resources") {
            match resolve(doc, value)? {
                Object::Null => {}
                found => return Ok(Some(found.as_dict()?)),
            }
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(parent)) => current = *parent,
            _ => return Ok(None),
        }
    }
    Err(PdfErr::Parse("page tree is too deep or cyclic".into()))
}

/// The dictionary under `key` in a resource dictionary. A null entry is the same as a missing one.
fn sub_dict<'a>(doc: &'a Document, res: &'a Dictionary, key: &[u8]) -> PResult<Option<&'a Dictionary>> {
    match res.get(key) {
        Ok(obj) => match resolve(doc, obj)? {
            Object::Null => Ok(None),
            found => Ok(Some(found.as_dict()?)),
        },
        Err(_) => Ok(None),
    }
}

/// A font resource: its dictionary, and the object number of the dictionary when the resource is an indirect object.
#[derive(Debug, Clone, Copy)]
struct FontRef<'a> {
    dict: &'a Dictionary,
    id: Option<ObjectId>,
}

/// The font entries of a resource dictionary, by resource name. The names are borrowed from the dictionary, so no name is copied.
/// The entries are resolved only when a `Tf` names one (`font_ref`), so an entry that no `Tf` selects is never checked. Each entry
/// is one unit of work.
type FontMap<'a> = BTreeMap<&'a [u8], &'a Object>;

fn font_map<'a>(doc: &'a Document, res: Option<&'a Dictionary>, meters: Meters<'_>) -> PResult<FontMap<'a>> {
    let mut out = BTreeMap::new();
    let Some(res) = res else {
        return Ok(out);
    };
    let Some(fonts) = sub_dict(doc, res, b"Font")? else {
        return Ok(out);
    };
    for (name, value) in fonts.iter() {
        meters.charge(Unit::Work, 1)?;
        out.insert(name.as_slice(), value);
    }
    Ok(out)
}

/// The font that a resource entry names: its dictionary, with its object number when it is indirect. `None` when the entry is
/// null, dangling, or not a dictionary.
fn font_ref<'a>(doc: &'a Document, value: &'a Object) -> Option<FontRef<'a>> {
    let dict = resolve(doc, value).ok()?.as_dict().ok()?;
    let id = final_id(doc, value).ok()?;
    Some(FontRef { dict, id })
}

fn num_obj(o: &Object) -> Option<f64> {
    crate::boxes::number(o)
}

/// An operator whose operands have the wrong number or type. The text, the form, or the value that it sets is unknown, so the page
/// is not read.
fn operand_error(operator: &str) -> PdfErr {
    PdfErr::invalid(
        "PDF_CONTENT_OPERAND",
        format!("the {operator} operator has the wrong number or type of operands; the page is not read"),
    )
}

/// An operator that takes no operands. An operand it does not take leaves its meaning unknown, so the page is not read.
fn no_operands(operator: &str, ops: &[Object]) -> PResult<()> {
    if ops.is_empty() {
        Ok(())
    } else {
        Err(operand_error(operator))
    }
}

/// The marked-content sequences of a content stream or form do not balance: an EMC has no open BDC or BMC, or a sequence is left
/// open. The page is not read, since the hidden content of the sequences cannot be told apart.
fn unbalanced_marks(reason: &str) -> PdfErr {
    PdfErr::invalid("PDF_MARKED_CONTENT_UNBALANCED", format!("{reason}; the page is not read"))
}

/// Reason of a default configuration that sets an optional content display this reader does not apply.
const AUTO_STATE_UNSUPPORTED: &str = "the optional content configuration has /AS, whose states depend on how the document is used";
const BASE_STATE_UNSUPPORTED: &str = "the optional content configuration has a /BaseState other than ON";
const MALFORMED_CONFIGURATION: &str = "the optional content configuration is malformed";

/// An optional content entry that this reader does not interpret. A page that uses it is not read.
fn unread_optional_content(reason: &str) -> PdfErr {
    PdfErr::unsupported("PDF_OPTIONAL_CONTENT_UNSUPPORTED", format!("{reason}; the page is not read"))
}

/// What a value leads to, followed through its references. Absent: a null, or a reference that names no object (a dangling reference
/// is the null object, ISO 32000-1 7.3.10). Broken: a reference cycle, or a chain longer than the reader follows, which names nothing
/// that can be read. Found: the value, and the number of the indirect object that holds it (None for a direct value).
enum Followed<'a> {
    Absent,
    Found(Option<ObjectId>, &'a Object),
    Broken,
}

/// Follows `value` through its references. The objects are read as they are stored, so a missing one is told apart from a cycle.
fn follow<'a>(doc: &'a Document, value: &'a Object) -> Followed<'a> {
    let mut id = None;
    let mut current = value;
    for _ in 0..crate::boxes::MAX_REFERENCE_HOPS {
        match current {
            Object::Reference(next) => match doc.objects.get(next) {
                Some(object) => {
                    id = Some(*next);
                    current = object;
                }
                None => return Followed::Absent,
            },
            Object::Null => return Followed::Absent,
            found => return Followed::Found(id, found),
        }
    }
    Followed::Broken
}

/// The entry `name` of `dict`, followed as `follow` does. A missing entry is absent.
fn dict_entry<'a>(doc: &'a Document, dict: &'a Dictionary, name: &[u8]) -> Followed<'a> {
    match dict.get(name) {
        Ok(value) => follow(doc, value),
        Err(_) => Followed::Absent,
    }
}

/// The default state of the optional content groups (ISO 32000-1 8.11.4): the `/D` configuration of the catalog's
/// `/OCProperties`. Only its /OFF list is applied, and only when its other entries are the defaults. A configuration with another
/// display (/AS, a /BaseState other than ON, a malformed entry, or a reference cycle) is kept as `uninterpreted`. A page that uses a
/// group under it is not read. A document with no configuration has every group ON.
#[derive(Debug, Default)]
struct OptionalContent {
    /// Object numbers of the groups that /OFF lists.
    off: HashSet<ObjectId>,
    /// The reason the configuration cannot be applied, when it has a display this reader does not interpret.
    uninterpreted: Option<&'static str>,
}

impl OptionalContent {
    /// Reads the default configuration. Each /OFF entry is charged to the meters. A null or a dangling reference is absent. A cycle,
    /// or a chain too long to follow, leaves the configuration uninterpreted.
    fn read(doc: &Document, meters: Meters<'_>) -> PResult<OptionalContent> {
        let mut config = OptionalContent::default();
        let Ok(catalog) = doc.catalog() else {
            return Ok(config);
        };
        let props = match dict_entry(doc, catalog, b"OCProperties") {
            Followed::Absent => return Ok(config),
            Followed::Broken => {
                config.refuse(MALFORMED_CONFIGURATION);
                return Ok(config);
            }
            Followed::Found(_, value) => value,
        };
        let Ok(props) = props.as_dict() else {
            config.refuse(MALFORMED_CONFIGURATION);
            return Ok(config);
        };
        // A configuration without /D leaves every group at its default, ON.
        let default = match dict_entry(doc, props, b"D") {
            Followed::Absent => return Ok(config),
            Followed::Broken => {
                config.refuse(MALFORMED_CONFIGURATION);
                return Ok(config);
            }
            Followed::Found(_, value) => value,
        };
        let Ok(default) = default.as_dict() else {
            config.refuse(MALFORMED_CONFIGURATION);
            return Ok(config);
        };
        match dict_entry(doc, default, b"AS") {
            Followed::Absent => {}
            Followed::Found(..) => config.refuse(AUTO_STATE_UNSUPPORTED),
            Followed::Broken => config.refuse(MALFORMED_CONFIGURATION),
        }
        match dict_entry(doc, default, b"BaseState") {
            Followed::Absent => {}
            Followed::Found(_, state) if state.as_name().is_ok_and(|name| name == b"ON") => {}
            Followed::Found(..) => config.refuse(BASE_STATE_UNSUPPORTED),
            Followed::Broken => config.refuse(MALFORMED_CONFIGURATION),
        }
        match dict_entry(doc, default, b"OFF") {
            Followed::Absent => {}
            Followed::Broken => config.refuse(MALFORMED_CONFIGURATION),
            Followed::Found(_, off) => {
                let Ok(items) = off.as_array() else {
                    config.refuse(MALFORMED_CONFIGURATION);
                    return Ok(config);
                };
                meters.charge(Unit::Entries, items.len() as u64)?;
                for item in items {
                    match follow(doc, item) {
                        // A null, or a reference that names no object, names no group.
                        Followed::Absent => {}
                        Followed::Broken => config.refuse(MALFORMED_CONFIGURATION),
                        Followed::Found(Some(id), _) => {
                            config.off.insert(id);
                        }
                        // A direct value is not a group, which must be an indirect object.
                        Followed::Found(None, _) => config.refuse(MALFORMED_CONFIGURATION),
                    }
                }
            }
        }
        Ok(config)
    }

    /// Keeps the first reason that the configuration cannot be applied.
    fn refuse(&mut self, reason: &'static str) {
        self.uninterpreted = self.uninterpreted.or(Some(reason));
    }
}

/// The value of a numeric operand. A non-number, or a number that is not finite, is an operand error.
fn number_operand(operator: &str, operand: &Object) -> PResult<f64> {
    num_obj(operand).filter(|v| v.is_finite()).ok_or_else(|| operand_error(operator))
}

/// The values of the numeric operands of an operator. The count must be exactly `N`: a missing operand and an extra one both leave
/// the operator's value unknown.
fn numbers<const N: usize>(operator: &str, ops: &[Object]) -> PResult<[f64; N]> {
    if ops.len() != N {
        return Err(operand_error(operator));
    }
    let mut values = [0.0; N];
    for (value, operand) in values.iter_mut().zip(ops) {
        *value = number_operand(operator, operand)?;
    }
    Ok(values)
}

fn matrix_from_slice(vals: &[Object]) -> Option<Matrix> {
    if vals.len() < 6 {
        return None;
    }
    let mut m = [0.0f64; 6];
    for (slot, value) in m.iter_mut().zip(vals.iter()) {
        *slot = num_obj(value)?;
    }
    if m.iter().any(|v| !v.is_finite()) {
        return None;
    }
    Some(Matrix(m))
}

struct Interp<'a> {
    doc: &'a Document,
    /// The fonts of the resources in scope. A form with resources of its own gets a map of its own, and any other form shares this one.
    fonts: Rc<FontMap<'a>>,
    xobjects: Option<&'a Dictionary>,
    /// The /Properties entry of the resources in scope, as it is stored. A form with resources of its own has its own, and any other
    /// form shares this one. It is followed only when a BDC names a property list (`property_hidden`).
    properties: Option<&'a Object>,
    /// Fonts that this page has loaded, by the address of their dictionary. A name is not a key: two names of one font share the entry,
    /// and one name in two scopes gives two entries.
    resolved: HashMap<usize, Rc<Font>>,
    /// Fonts whose missing widths are already counted on this page, by address.
    counted_widths: HashSet<usize>,
    /// Fonts whose ignored ToUnicode entries are already counted on this page, by address.
    counted_ignored: HashSet<usize>,
    /// Fonts whose shown text came from their encoding, already counted on this page, by address.
    counted_encoding: HashSet<usize>,
    /// Fonts whose shown text came from StandardEncoding in place of a Type 1 built-in encoding that was not read, already counted
    /// on this page, by address.
    counted_type1: HashSet<usize>,
    st: GState<'a>,
    /// The graphics states that the open `q` operators saved. A form XObject has a stack of its own.
    stack: Vec<GState<'a>>,
    tm: Matrix,
    tlm: Matrix,
    clips: Vec<[f64; 4]>,
    out: Vec<RawGlyph>,
    depth: u32,
    /// Non-fatal findings by diagnostic code, with how many times each occurred. No document text is kept.
    warnings: BTreeMap<&'static str, u32>,
    images: u32,
    /// The page's own counts. The request's counts are in `request`.
    page: Meter,
    request: &'a Request,
    /// Decoded operators of each form stream, by object number. A form that is run again is not decoded again.
    forms: HashMap<ObjectId, Rc<Vec<Operation>>>,
    /// The fonts of each form's own resources, by object number. The map is built on the form's first run on this page, so its
    /// entries are charged once, and the later runs share it.
    form_fonts: HashMap<ObjectId, Rc<FontMap<'a>>>,
    /// The open marked-content sequences of the page and of the forms around the current one, outermost first. True marks a
    /// sequence whose optional content is hidden.
    marks: Vec<bool>,
    /// How many sequences in `marks` are hidden. Glyphs are left out of the evidence while it is not zero.
    hidden_marks: usize,
    /// Where the sequences of the form now running start in `marks`. Its EMC operators cannot close the sequences below it.
    mark_base: usize,
    /// Form streams decoded, for the tests of the cache.
    #[cfg(test)]
    decodes: u64,
}

/// A non-fatal finding: its stable diagnostic code and how many times it occurred on the page.
pub(crate) type Warning = (&'static str, u32);

impl<'a> Interp<'a> {
    /// Counts one occurrence of a non-fatal finding.
    fn note(&mut self, code: &'static str) {
        let count = self.warnings.entry(code).or_insert(0);
        *count = count.saturating_add(1);
    }

    fn meters(&self) -> Meters<'_> {
        Meters { page: &self.page, request: &self.request.meter }
    }

    fn charge(&self, unit: Unit, n: u64) -> PResult<()> {
        self.meters().charge(unit, n)
    }

    /// The font that a `Tf` named. A font that is an indirect object is loaded once per request, under its object number. Any other
    /// font is loaded once per page, under the address of its dictionary, which does not move while the document is borrowed.
    fn font(&mut self, entry: FontRef<'a>) -> PResult<Rc<Font>> {
        let key = entry.dict as *const Dictionary as usize;
        if let Some(font) = self.resolved.get(&key) {
            return Ok(Rc::clone(font));
        }
        let font = match entry.id {
            Some(id) => {
                let loaded = self.request.fonts.borrow().get(&id).map(Rc::clone);
                match loaded {
                    Some(font) => font,
                    None => {
                        let font = Rc::new(Font::load(self.doc, entry.dict, self.meters())?);
                        self.request.fonts.borrow_mut().insert(id, Rc::clone(&font));
                        font
                    }
                }
            }
            None => Rc::new(Font::load(self.doc, entry.dict, self.meters())?),
        };
        if !font.width_known && self.counted_widths.insert(Rc::as_ptr(&font) as usize) {
            self.note("PDF_WIDTH_UNKNOWN");
        }
        if font.ignored_tounicode_entries() > 0 && self.counted_ignored.insert(Rc::as_ptr(&font) as usize) {
            self.note("PDF_TOUNICODE_ENTRY_IGNORED");
        }
        self.resolved.insert(key, Rc::clone(&font));
        Ok(font)
    }

    /// The fonts of a form's own resources. A form that has an object number builds the map on its first run on this page, and the
    /// later runs share it. The map's entries are charged when it is built, and each run charges only what it runs.
    fn own_font_map(&mut self, form: Option<ObjectId>, res: &'a Dictionary) -> PResult<Rc<FontMap<'a>>> {
        if let Some(map) = form.and_then(|id| self.form_fonts.get(&id)) {
            return Ok(Rc::clone(map));
        }
        let map = Rc::new(font_map(self.doc, Some(res), self.meters())?);
        if let Some(id) = form {
            self.form_fonts.insert(id, Rc::clone(&map));
        }
        Ok(map)
    }

    /// The default optional content configuration of the document. The first use in the request reads it, and later uses share it.
    fn optional_content(&self) -> PResult<Rc<OptionalContent>> {
        if let Some(config) = self.request.optional.borrow().as_ref() {
            return Ok(Rc::clone(config));
        }
        let config = Rc::new(OptionalContent::read(self.doc, self.meters())?);
        *self.request.optional.borrow_mut() = Some(Rc::clone(&config));
        Ok(config)
    }

    /// Whether the content that an optional content entry controls is hidden. `value` is a `/OC` entry of an XObject, or the property
    /// list that a BDC names. Null and a dangling reference are absent, so the content is shown. A group is hidden when the default
    /// configuration lists it in /OFF. A membership dictionary (OCMD), a direct value, a reference cycle, or a configuration that this
    /// reader does not apply fails the page.
    fn hidden_by(&self, value: &Object) -> PResult<bool> {
        let (id, object) = match follow(self.doc, value) {
            Followed::Absent => return Ok(false),
            Followed::Broken => {
                return Err(unread_optional_content("an optional content reference is a cycle or a chain too long to follow"))
            }
            Followed::Found(None, _) => {
                return Err(unread_optional_content("an optional content entry is a direct object, not a reference to a group"))
            }
            Followed::Found(Some(id), object) => (id, object),
        };
        let Object::Dictionary(dict) = object else {
            return Err(unread_optional_content("an optional content entry is not a dictionary"));
        };
        let kind = match dict_entry(self.doc, dict, b"Type") {
            Followed::Found(_, kind) => kind.as_name().ok(),
            Followed::Absent | Followed::Broken => None,
        };
        match kind {
            Some(b"OCG") => {}
            Some(b"OCMD") => return Err(unread_optional_content("a membership dictionary (OCMD) is not interpreted")),
            _ => return Err(unread_optional_content("an optional content entry is not an optional content group")),
        }
        let config = self.optional_content()?;
        if let Some(reason) = config.uninterpreted {
            return Err(unread_optional_content(reason));
        }
        Ok(config.off.contains(&id))
    }

    /// Whether the property list that a BDC names is hidden. Only the /OC tag reads it. The /Properties in scope is followed here and
    /// nowhere else, so a damaged one fails only the pages that name a property list. A name that is not there names nothing.
    fn property_hidden(&self, props: &Object) -> PResult<bool> {
        match props {
            Object::Name(name) => {
                let Some(list) = self.properties else {
                    return Ok(false);
                };
                let entries = match follow(self.doc, list) {
                    Followed::Absent => return Ok(false),
                    Followed::Broken => {
                        return Err(unread_optional_content("the /Properties of the resources is a cycle or a chain too long to follow"))
                    }
                    Followed::Found(_, value) => value
                        .as_dict()
                        .map_err(|_| unread_optional_content("the /Properties of the resources is not a dictionary"))?,
                };
                match entries.get(name) {
                    Ok(value) => self.hidden_by(value),
                    Err(_) => Ok(false),
                }
            }
            Object::Dictionary(_) => Err(unread_optional_content("an optional content property list is written inline")),
            _ => Err(operand_error("BDC")),
        }
    }

    /// Opens a marked-content sequence. Its content is hidden when `hidden` is set.
    fn open_mark(&mut self, hidden: bool) -> PResult<()> {
        if self.marks.len() >= MAX_MARKED_CONTENT_DEPTH {
            return Err(PdfErr::unsupported(
                "PDF_MARKED_CONTENT_DEPTH",
                "marked content is nested deeper than the supported depth; the page is not read",
            ));
        }
        self.marks.push(hidden);
        self.hidden_marks += usize::from(hidden);
        Ok(())
    }

    /// Closes the innermost sequence that the current form or page opened. An EMC with none open leaves the page unread.
    fn close_mark(&mut self) -> PResult<()> {
        if self.marks.len() <= self.mark_base {
            return Err(unbalanced_marks("an EMC operator has no open BDC or BMC"));
        }
        if self.marks.pop() == Some(true) {
            self.hidden_marks -= 1;
        }
        Ok(())
    }

    fn show(&mut self, bytes: &[u8]) -> PResult<()> {
        let entry = match self.st.font {
            FontSlot::Unset => {
                return Err(PdfErr::invalid("PDF_TEXT_BEFORE_FONT", "text is shown before Tf sets a font"));
            }
            FontSlot::Missing => {
                return Err(PdfErr::invalid("PDF_FONT_MISSING", "a font that Tf names is missing or is not a font dictionary"));
            }
            FontSlot::Found(entry) => entry,
        };
        let font = self.font(entry)?;
        let font_key = Rc::as_ptr(&font) as usize;
        let tfs = self.st.tfs;
        let th = self.st.th;
        let rise = self.st.rise;
        for (code, nbytes) in font.codes(bytes) {
            // The text matrix advances after every glyph, so each glyph reads the current Tm.
            let text_to_user = self.tm.mul(self.st.ctm);
            let eff_size = tfs * text_to_user.det().abs().sqrt();
            let w_em = font.width(code) / 1000.0;
            // A glyph in hidden optional content is left out of the evidence. It still moves the text position, so it is advanced below.
            if self.hidden_marks == 0 {
                let text = font.text(code)?;
                if font.text_from_encoding(code) && self.counted_encoding.insert(font_key) {
                    self.note("PDF_TEXT_FROM_ENCODING");
                }
                if font.text_from_standard_for_builtin(code) && self.counted_type1.insert(font_key) {
                    self.note("PDF_TYPE1_ENCODING_UNREADABLE");
                }
                // Glyph space (em) to user space: scale by Tfs and Th, add Rise, then Tm and CTM.
                let glyph_to_user = Matrix([tfs * th, 0.0, 0.0, tfs, 0.0, rise]).mul(text_to_user);
                let (ox, oy) = glyph_to_user.apply(0.0, 0.0);
                let corners = [
                    glyph_to_user.apply(0.0, font.descent / 1000.0),
                    glyph_to_user.apply(w_em, font.descent / 1000.0),
                    glyph_to_user.apply(0.0, font.ascent / 1000.0),
                    glyph_to_user.apply(w_em, font.ascent / 1000.0),
                ];
                let cell = [
                    corners.iter().map(|c| c.0).fold(f64::INFINITY, f64::min),
                    corners.iter().map(|c| c.1).fold(f64::INFINITY, f64::min),
                    corners.iter().map(|c| c.0).fold(f64::NEG_INFINITY, f64::max),
                    corners.iter().map(|c| c.1).fold(f64::NEG_INFINITY, f64::max),
                ];
                let clipped = self.clips.iter().any(|c| ox < c[0] || ox > c[2] || oy < c[1] || oy > c[3]);
                self.charge(Unit::Glyphs, 1)?;
                self.out.push(RawGlyph {
                    text,
                    origin_x: ox,
                    origin_y: oy,
                    cell,
                    font_size: eff_size,
                    render_mode: self.st.tr,
                    clipped,
                    width_known: font.width_known,
                });
            }
            let mut tx = (w_em * tfs + self.st.tc) * th;
            if nbytes == 1 && code == 32 {
                tx += self.st.tw * th;
            }
            self.tm = Matrix::translate(tx, 0.0).mul(self.tm);
        }
        Ok(())
    }

    fn next_line(&mut self) {
        self.tlm = Matrix::translate(0.0, -self.st.tl).mul(self.tlm);
        self.tm = self.tlm;
    }

    /// Runs a Form XObject. Other XObjects (images) carry no text and are skipped.
    fn do_xobject(&mut self, name: &[u8]) -> PResult<()> {
        let entry = self.xobjects.and_then(|xobjs| xobjs.get(name).ok());
        // A null entry, or a reference that names no object, is the same as a missing one (ISO 32000-1 7.3.10). A reference cycle
        // or a chain that is too long is not judged here: `resolve` below fails the page for it.
        let entry = entry.filter(|&entry| !matches!(follow(self.doc, entry), Followed::Absent));
        let Some(entry) = entry else {
            // Nothing is drawn for a name with no resource. The page reports how often that happened, not the name.
            self.note("PDF_XOBJECT_MISSING");
            return Ok(());
        };
        let stream = resolve(self.doc, entry)?.as_stream()?;
        // An XObject whose optional content group is OFF is not drawn, so it is not read. A group that cannot be applied fails the page.
        if let Ok(oc) = stream.dict.get(b"OC") {
            if self.hidden_by(oc)? {
                return Ok(());
            }
        }
        // /Subtype may be an indirect reference, so it is read through the reference.
        let subtype = stream.dict.get_deref(b"Subtype", self.doc).ok().and_then(|o| o.as_name().ok());
        match subtype {
            Some(b"Form") => {}
            Some(b"Image") => {
                self.images += 1;
                return Ok(());
            }
            Some(b"PS") => {
                // A PostScript XObject (ISO 32000-1 8.8.2) is not executed by PDF readers and draws no PDF text, so the
                // page stays read. Its uses are counted.
                self.note("PDF_XOBJECT_POSTSCRIPT");
                return Ok(());
            }
            _ => {
                // Only forms, images and PostScript XObjects are handled. Any other subtype, or none, leaves the content unread, so the page is not read.
                return Err(PdfErr::unsupported(
                    "PDF_XOBJECT_UNKNOWN",
                    "an XObject whose subtype is neither Form nor Image is used; the page is not read",
                ));
            }
        }
        if self.depth >= MAX_FORM_DEPTH {
            return Err(PdfErr::unsupported("PDF_FORM_DEPTH", "form XObject nesting exceeds the supported depth"));
        }
        // The depth limit bounds nesting only. Calls that fan out are bounded by the runs counted on the page and the request.
        self.charge(Unit::FormRuns, 1)?;
        let form_matrix = stream
            .dict
            .get(b"Matrix")
            .ok()
            .and_then(|o| resolve(self.doc, o).ok())
            .and_then(|o| o.as_array().ok())
            .and_then(|arr| matrix_from_slice(arr))
            .unwrap_or(Matrix::IDENTITY);
        // A null /Resources is the same as a missing one, so the form then uses the fonts and XObjects in scope.
        let own_res: Option<&'a Dictionary> = match stream.dict.get(b"Resources") {
            Ok(obj) => match resolve(self.doc, obj)? {
                Object::Null => None,
                res => Some(res.as_dict()?),
            },
            Err(_) => None,
        };
        // A form stream is decoded once per page, under the object number that its reference finally names. Its operators do
        // not depend on where it is run. Its resources do not depend on it either, so its fonts are keyed the same way.
        let form_id = final_id(self.doc, entry)?;
        // A form without resources of its own uses the fonts in scope. Those are shared, not copied for the run.
        let (new_fonts, new_xobjs, new_props) = match own_res {
            Some(res) => (
                self.own_font_map(form_id, res)?,
                sub_dict(self.doc, res, b"XObject")?,
                res.get(b"Properties").ok(),
            ),
            None => (Rc::clone(&self.fonts), self.xobjects, self.properties),
        };
        let operations = match form_id.and_then(|id| self.forms.get(&id)) {
            Some(cached) => Rc::clone(cached),
            None => {
                let data = decode_complete(stream, self.doc, self.meters())?;
                let decoded = Rc::new(decode_operations_within(&data, self.meters())?);
                #[cfg(test)]
                {
                    self.decodes += 1;
                }
                if let Some(id) = form_id {
                    self.forms.insert(id, Rc::clone(&decoded));
                }
                decoded
            }
        };

        let new_ctm = form_matrix.mul(self.st.ctm);
        // /BBox is in form space. Its AABB in user space (after the form matrix and CTM) clips glyph origins.
        let clip = match stream.dict.get(b"BBox") {
            Ok(obj) => {
                let arr = resolve(self.doc, obj)?.as_array()?;
                let mut vals = [0.0f64; 4];
                let mut count = 0;
                for item in arr.iter().take(4) {
                    if let Some(v) = num_obj(resolve(self.doc, item)?) {
                        vals[count] = v;
                        count += 1;
                    }
                }
                if count == 4 {
                    let corners = [
                        new_ctm.apply(vals[0], vals[1]),
                        new_ctm.apply(vals[2], vals[1]),
                        new_ctm.apply(vals[0], vals[3]),
                        new_ctm.apply(vals[2], vals[3]),
                    ];
                    Some([
                        corners.iter().map(|c| c.0).fold(f64::INFINITY, f64::min),
                        corners.iter().map(|c| c.1).fold(f64::INFINITY, f64::min),
                        corners.iter().map(|c| c.0).fold(f64::NEG_INFINITY, f64::max),
                        corners.iter().map(|c| c.1).fold(f64::NEG_INFINITY, f64::max),
                    ])
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        let saved_fonts = std::mem::replace(&mut self.fonts, new_fonts);
        let saved_xobjs = std::mem::replace(&mut self.xobjects, new_xobjs);
        let saved_props = std::mem::replace(&mut self.properties, new_props);
        let saved_base = std::mem::replace(&mut self.mark_base, self.marks.len());
        // The form's q and Q are its own. A q that the form leaves open must not change the page's stack.
        let saved_stack = std::mem::take(&mut self.stack);
        let saved_st = self.st;
        let saved_tm = self.tm;
        let saved_tlm = self.tlm;
        let clip_len = self.clips.len();
        if let Some(c) = clip {
            self.clips.push(c);
        }
        self.st.ctm = new_ctm;
        self.depth += 1;
        let mut result = self.run_ops(&operations);
        // A form closes every sequence that it opens. The ones open when it started are not its own, and it must leave them as they were.
        if result.is_ok() && self.marks.len() != self.mark_base {
            result = Err(unbalanced_marks("a form XObject leaves marked content open"));
        }
        self.depth -= 1;
        self.clips.truncate(clip_len);
        self.fonts = saved_fonts;
        self.xobjects = saved_xobjs;
        self.properties = saved_props;
        self.mark_base = saved_base;
        self.stack = saved_stack;
        self.st = saved_st;
        self.tm = saved_tm;
        self.tlm = saved_tlm;
        result
    }

    fn run_ops(&mut self, ops: &[Operation]) -> PResult<()> {
        for op in ops {
            self.charge(Unit::Work, 1)?;
            self.run(&op.operator, &op.operands)?;
        }
        Ok(())
    }

    fn run(&mut self, operator: &str, ops: &[Object]) -> PResult<()> {
        match operator {
            "q" => {
                if self.stack.len() >= MAX_GRAPHICS_STATE_DEPTH {
                    return Err(PdfErr::unsupported(
                        "PDF_GSTATE_DEPTH",
                        "graphics states are nested deeper than the supported depth for one content stream",
                    ));
                }
                self.stack.push(self.st);
            }
            "Q" => {
                if let Some(s) = self.stack.pop() {
                    self.st = s;
                }
            }
            "cm" => {
                let [a, b, c, d, e, f] = numbers::<6>("cm", ops)?;
                self.st.ctm = Matrix([a, b, c, d, e, f]).mul(self.st.ctm);
            }
            "BT" => {
                no_operands("BT", ops)?;
                self.tm = Matrix::IDENTITY;
                self.tlm = Matrix::IDENTITY;
            }
            "Tc" => self.st.tc = numbers::<1>("Tc", ops)?[0],
            "Tw" => self.st.tw = numbers::<1>("Tw", ops)?[0],
            "Tz" => self.st.th = numbers::<1>("Tz", ops)?[0] / 100.0,
            "TL" => self.st.tl = numbers::<1>("TL", ops)?[0],
            "Ts" => self.st.rise = numbers::<1>("Ts", ops)?[0],
            "Tr" => self.st.tr = numbers::<1>("Tr", ops)?[0] as u8,
            "Tf" => match ops {
                [Object::Name(name), size] => {
                    let size = number_operand("Tf", size)?;
                    // The name is looked up now, in the resources in scope, and the font it names is fixed by this operator.
                    self.st.font = match self.fonts.get(name.as_slice()) {
                        Some(value) => match font_ref(self.doc, *value) {
                            Some(entry) => FontSlot::Found(entry),
                            None => FontSlot::Missing,
                        },
                        None => FontSlot::Missing,
                    };
                    self.st.tfs = size;
                }
                _ => return Err(operand_error("Tf")),
            },
            "Td" => {
                let [x, y] = numbers::<2>("Td", ops)?;
                self.tlm = Matrix::translate(x, y).mul(self.tlm);
                self.tm = self.tlm;
            }
            "TD" => {
                let [x, y] = numbers::<2>("TD", ops)?;
                self.st.tl = -y;
                self.tlm = Matrix::translate(x, y).mul(self.tlm);
                self.tm = self.tlm;
            }
            "Tm" => {
                let [a, b, c, d, e, f] = numbers::<6>("Tm", ops)?;
                self.tlm = Matrix([a, b, c, d, e, f]);
                self.tm = self.tlm;
            }
            "T*" => {
                no_operands("T*", ops)?;
                self.next_line();
            }
            // An operand of the wrong number or type would skip the text or the form that the operator draws, so the page is not read.
            "Tj" => match ops {
                [Object::String(s, _)] => self.show(s)?,
                _ => return Err(operand_error("Tj")),
            },
            "TJ" => {
                let [Object::Array(items)] = ops else {
                    return Err(operand_error("TJ"));
                };
                for item in items {
                    self.charge(Unit::Work, 1)?;
                    match item {
                        Object::String(s, _) => self.show(s)?,
                        other => {
                            // Every glyph after a non-finite adjustment would get a non-finite position, so it is an operand error.
                            let adj = number_operand("TJ", other)?;
                            let tx = -adj / 1000.0 * self.st.tfs * self.st.th;
                            self.tm = Matrix::translate(tx, 0.0).mul(self.tm);
                        }
                    }
                }
            }
            "'" => match ops {
                [Object::String(s, _)] => {
                    self.next_line();
                    self.show(s)?;
                }
                _ => return Err(operand_error("'")),
            },
            "\"" => match ops {
                [aw, ac, Object::String(s, _)] => {
                    self.st.tw = number_operand("\"", aw)?;
                    self.st.tc = number_operand("\"", ac)?;
                    self.next_line();
                    self.show(s)?;
                }
                _ => return Err(operand_error("\"")),
            },
            "Do" => match ops {
                [Object::Name(name)] => self.do_xobject(name)?,
                _ => return Err(operand_error("Do")),
            },
            "ET" => no_operands("ET", ops)?,
            // A marked-content sequence. Only /OC names optional content: its property list is a group, a membership, or nothing.
            "BMC" => match ops {
                [Object::Name(_)] => self.open_mark(false)?,
                _ => return Err(operand_error("BMC")),
            },
            "BDC" => match ops {
                [Object::Name(tag), props @ (Object::Name(_) | Object::Dictionary(_))] => {
                    let hidden = tag.as_slice() == b"OC" && self.property_hidden(props)?;
                    self.open_mark(hidden)?;
                }
                _ => return Err(operand_error("BDC")),
            },
            "EMC" => {
                no_operands("EMC", ops)?;
                self.close_mark()?;
            }
            // Each bounded inline image is one "BI" operation. `decode_operations` checks that these are the bounded images.
            "BI" => self.images += 1,
            _ => {}
        }
        Ok(())
    }
}

/// Decoded content of all page content streams, in order. A damaged stream is an error, never a readable prefix.
pub(crate) fn page_content_bytes(doc: &Document, page_id: ObjectId, meters: Meters<'_>) -> PResult<Vec<u8>> {
    let mut out = Vec::new();
    for id in content_ids(doc, page_id, meters)? {
        let stream = doc.get_object(id)?.as_stream()?;
        let data = decode_complete(stream, doc, meters)?;
        // The line feed after each stream is output too, so it is charged like the decoded bytes.
        meters.charge(Unit::Decoded, 1)?;
        out.extend_from_slice(&data);
        out.push(b'\n');
    }
    Ok(out)
}

/// The content streams of a page, in order. The references are walked as lopdf's `Document::get_page_contents` walks them, except
/// where it is unsafe. An array's length is checked before the array is copied into a list, and each entry is charged. A direct
/// entry that is not null is refused: lopdf would skip it, and the page would then be read as complete without that stream.
fn content_ids(doc: &Document, page_id: ObjectId, meters: Meters<'_>) -> PResult<Vec<ObjectId>> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Ok(Vec::new());
    };
    let Ok(mut contents) = page.get(b"Contents") else {
        return Ok(Vec::new());
    };
    let mut hops = 0usize;
    while let Object::Reference(id) = contents {
        match doc.objects.get(id) {
            // A missing object is listed as well, so that reading it fails the page as a missing stream does.
            None | Some(Object::Stream(_)) => return Ok(vec![*id]),
            Some(next) if hops + 1 < crate::boxes::MAX_REFERENCE_HOPS => {
                hops += 1;
                contents = next;
            }
            Some(_) => return Err(PdfErr::Parse("a /Contents reference chain is too long or loops".into())),
        }
    }
    match contents {
        Object::Array(entries) => {
            if entries.len() > MAX_CONTENT_STREAMS {
                return Err(PdfErr::unsupported(
                    "PDF_CONTENT_BUDGET",
                    "the page lists more content streams than the supported count for one page",
                ));
            }
            let mut ids = Vec::with_capacity(entries.len());
            for entry in entries {
                meters.charge(Unit::Work, 1)?;
                match entry {
                    Object::Reference(id) => ids.push(*id),
                    Object::Null => {}
                    _ => return Err(contents_invalid()),
                }
            }
            Ok(ids)
        }
        Object::Null => Ok(Vec::new()),
        _ => Err(contents_invalid()),
    }
}

fn contents_invalid() -> PdfErr {
    PdfErr::invalid(
        "PDF_CONTENTS_INVALID",
        "a /Contents entry is not an indirect reference to a content stream; the page is not read",
    )
}

/// Operators of one content stream. Every inline image is bounded first (`bound_inline_images`), before any strict parse.
/// The page is an error unless each image's data end is proven. The bounded copy is then parsed strictly, and the `BI`
/// operators it yields must be exactly the bounded images, so lopdf's own inline image end is never trusted. If the
/// strict parse fails, the separators are rewritten and parsed again. lopdf's lenient decoder would return the operators
/// before the first error as if they were all, so it is not used.
/// `decode_operations` under the given meters. The tokens are charged to them.
fn decode_operations_within(data: &[u8], meters: Meters<'_>) -> PResult<Vec<Operation>> {
    let (bounded, images) = bound_inline_images(data)?;
    // The tokens are counted before the parse, so a stream over the limit never has its operators built.
    meters.charge(Unit::Tokens, count_tokens(&bounded))?;
    let operations = match Content::decode_strict(&bounded) {
        Ok(content) => content.operations,
        Err(_) => match Content::decode_strict(&with_separators_as_space(&bounded)) {
            Ok(content) => content.operations,
            Err(_) => {
                return Err(PdfErr::invalid(
                    "PDF_CONTENT_TRUNCATED",
                    "the content stream does not parse to its end; the page is not read",
                ))
            }
        },
    };
    check_inline_images(&operations, images)?;
    Ok(operations)
}

/// Operators of one content stream, with the operator-count checks of `decode_operations_within`, under the production limits.
#[cfg(test)]
fn decode_operations(data: &[u8]) -> PResult<Vec<Operation>> {
    let page = Meter::page(PAGE_BUDGET);
    let request = Meter::request(REQUEST_BUDGET);
    decode_operations_within(data, Meters { page: &page, request: &request })
}

/// An upper bound on the objects and operators that lopdf builds from `data`, which holds no inline image data. A token is a name,
/// a literal or hex string, a `[` or a `<<`, or a word. A word counts once when it is a plain number, a plain operator, or
/// `true`, `false` or `null`. Any other word counts once per byte, because lopdf may split it into several objects and each of
/// them takes at least one byte. Comments, white space and the closing delimiters add nothing.
fn count_tokens(data: &[u8]) -> u64 {
    let mut count = 0u64;
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        if is_pdf_space(b) {
            i += 1;
        } else if b == b'%' {
            i = comment_end(data, i);
        } else if b == b'(' {
            i = literal_string_end(data, i);
            count += 1;
        } else if b == b'<' && data.get(i + 1) == Some(&b'<') {
            i += 2;
            count += 1;
        } else if b == b'<' {
            i = hex_string_end(data, i);
            count += 1;
        } else if b == b'[' {
            i += 1;
            count += 1;
        } else if b == b'/' {
            i = token_end(data, i + 1);
            count += 1;
        } else if is_delimiter(b) {
            i += if b == b'>' && data.get(i + 1) == Some(&b'>') { 2 } else { 1 };
        } else {
            let end = token_end(data, i);
            count += word_tokens(&data[i..end]);
            i = end;
        }
    }
    count
}

/// The tokens of one word, as `count_tokens` counts them.
fn word_tokens(word: &[u8]) -> u64 {
    let plain_number = word.iter().any(u8::is_ascii_digit)
        && word.iter().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.'))
        && word.iter().skip(1).all(|b| !matches!(b, b'+' | b'-'))
        && word.iter().filter(|&&b| b == b'.').count() <= 1;
    let plain_operator = word.iter().all(|b| b.is_ascii_alphabetic() || matches!(b, b'*' | b'\'' | b'"'))
        && !(word.starts_with(b"true") || word.starts_with(b"false") || word.starts_with(b"null"));
    let keyword = matches!(word, b"true" | b"false" | b"null");
    if plain_number || plain_operator || keyword {
        1
    } else {
        word.len() as u64
    }
}

/// Bytes to parse in place of `data`. PDF whitespace is NUL, HT, LF, FF, CR and SP, but lopdf's content parser accepts
/// only HT, LF, CR and SP between operators. So NUL and FF become SP, and so do comments, which PDF allows between
/// operands and lopdf does not. Literal strings and hex strings are skipped, so their bytes never change. Each rewritten
/// byte becomes one SP, so no offset moves. Inline images are already bounded, so no image data is seen here.
fn with_separators_as_space(data: &[u8]) -> Cow<'_, [u8]> {
    let mut rewrite: Vec<Range<usize>> = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        if b == b'(' {
            i = literal_string_end(data, i);
        } else if b == b'<' && data.get(i + 1) == Some(&b'<') {
            i += 2;
        } else if b == b'<' {
            i = hex_string_end(data, i);
        } else if b == b'%' {
            let end = comment_end(data, i);
            rewrite.push(i..end);
            i = end;
        } else if b == b'/' {
            i = token_end(data, i + 1);
        } else {
            if b == 0x00 || b == 0x0C {
                rewrite.push(i..i + 1);
            }
            i += 1;
        }
    }
    if rewrite.is_empty() {
        return Cow::Borrowed(data);
    }
    let mut out = data.to_vec();
    for range in rewrite {
        out[range].fill(b' ');
    }
    Cow::Owned(out)
}

/// PDF whitespace characters (ISO 32000-1, 7.2.2).
fn is_pdf_space(b: u8) -> bool {
    matches!(b, 0x00 | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

/// PDF delimiter characters (ISO 32000-1, 7.2.2).
fn is_delimiter(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// End of the token that starts at `start`.
fn token_end(data: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < data.len() && !is_pdf_space(data[end]) && !is_delimiter(data[end]) {
        end += 1;
    }
    end
}

/// Name bytes after the `/` with `#xx` escapes decoded (ISO 32000-1, 7.3.5). `None` when an escape is not two hex digits,
/// or when it decodes to NUL, which a name may not contain.
fn decode_name(raw: &[u8]) -> Option<Cow<'_, [u8]>> {
    if !raw.contains(&b'#') {
        return Some(Cow::Borrowed(raw));
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'#' {
            let high = hex_digit(*raw.get(i + 1)?)?;
            let low = hex_digit(*raw.get(i + 2)?)?;
            let byte = (high << 4) | low;
            if byte == 0 {
                return None;
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    Some(Cow::Owned(out))
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// End of the literal string that starts at `start` (`(`). Nested parentheses are counted, and a backslash escapes the
/// byte after it.
fn literal_string_end(data: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < data.len() {
        match data[i] {
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    data.len()
}

/// End of the hex string that starts at `start` (`<`). Its NUL and FF bytes are left as they are: lopdf's hex parser
/// skips them.
fn hex_string_end(data: &[u8], start: usize) -> usize {
    data[start..].iter().position(|&b| b == b'>').map_or(data.len(), |p| start + p + 1)
}

/// End of the comment that starts at `start` (`%`): the CR or LF that ends it, or the end of the data.
fn comment_end(data: &[u8], start: usize) -> usize {
    data[start..].iter().position(|&b| b == b'\r' || b == b'\n').map_or(data.len(), |p| start + p)
}

/// Canonical stand-in for an inline image whose data end was proven. lopdf reads it exactly: one data byte, `0`, in a
/// one-component 8-bit image. The original data never reaches the parser, so its bytes cannot act as operators.
const CANONICAL_IMAGE: &[u8] = b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID 0 EI";
const CANONICAL_IMAGE_DATA: &[u8] = b"0";

/// Replaces every inline image in `data` by `CANONICAL_IMAGE` once its data end is proven, and returns the result with the
/// number of images. Literal strings, hex strings, comments and names are skipped, as in `with_separators_as_space`. One
/// image that cannot be bounded fails the whole stream.
fn bound_inline_images(data: &[u8]) -> PResult<(Cow<'_, [u8]>, usize)> {
    let mut out: Vec<u8> = Vec::new();
    let mut copied = 0usize;
    let mut images = 0usize;
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        if b == b'(' {
            i = literal_string_end(data, i);
        } else if b == b'<' && data.get(i + 1) == Some(&b'<') {
            i += 2;
        } else if b == b'<' {
            i = hex_string_end(data, i);
        } else if b == b'%' {
            i = comment_end(data, i);
        } else if b == b'/' {
            i = token_end(data, i + 1);
        } else if is_pdf_space(b) || is_delimiter(b) {
            i += 1;
        } else {
            let end = token_end(data, i);
            if &data[i..end] == b"BI" {
                let image_end = inline_image_end(data, end)?;
                out.extend_from_slice(&data[copied..i]);
                out.extend_from_slice(CANONICAL_IMAGE);
                copied = image_end;
                images += 1;
                i = image_end;
            } else {
                i = end;
            }
        }
    }
    if images == 0 {
        return Ok((Cow::Borrowed(data), 0));
    }
    out.extend_from_slice(&data[copied..]);
    Ok((Cow::Owned(out), images))
}

/// The operators must be exactly the bounded images, each one lopdf returned as the canonical image. Anything else fails
/// the page, for example a bare `BI` from a lopdf fallback that skipped data up to a first `EI`.
fn check_inline_images(operations: &[Operation], images: usize) -> PResult<()> {
    let mut found = 0usize;
    for operation in operations.iter().filter(|op| op.operator == "BI") {
        found += 1;
        let canonical = matches!(
            operation.operands.as_slice(),
            [Object::Stream(stream)] if stream.content.as_slice() == CANONICAL_IMAGE_DATA
        );
        if !canonical {
            return Err(boundary_error("the parser did not return the bounded image"));
        }
    }
    if found != images {
        return Err(boundary_error("the parser did not return every bounded image"));
    }
    Ok(())
}

/// Dictionary entries of one inline image that decide its data length. Each is the byte range of the entry's value.
#[derive(Default)]
struct InlineEntries {
    width: Option<(usize, usize)>,
    height: Option<(usize, usize)>,
    bpc: Option<(usize, usize)>,
    image_mask: Option<(usize, usize)>,
    colour_space: Option<(usize, usize)>,
}

/// Keys of an inline image dictionary whose meaning is known. A full name and its abbreviation are the same key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineKey {
    Width,
    Height,
    BitsPerComponent,
    ImageMask,
    ColourSpace,
    Decode,
    DecodeParms,
    Interpolate,
    Filter,
}

impl InlineKey {
    /// The key that a name spells, after `#xx` decoding. `None` for any other name.
    fn from_name(name: &[u8]) -> Option<InlineKey> {
        Some(match name {
            b"W" | b"Width" => InlineKey::Width,
            b"H" | b"Height" => InlineKey::Height,
            b"BPC" | b"BitsPerComponent" => InlineKey::BitsPerComponent,
            b"IM" | b"ImageMask" => InlineKey::ImageMask,
            b"CS" | b"ColorSpace" => InlineKey::ColourSpace,
            b"D" | b"Decode" => InlineKey::Decode,
            b"DP" | b"DecodeParms" => InlineKey::DecodeParms,
            b"I" | b"Interpolate" => InlineKey::Interpolate,
            b"F" | b"Filter" => InlineKey::Filter,
            _ => return None,
        })
    }
}

/// End of the inline image whose `BI` operator ends at `bi_end`, as the offset just after its `EI`. The image is accepted
/// only when the dictionary gives its data length and `EI` follows exactly that many bytes (see `proven_end`). Data bytes
/// are never scanned, so an `EI` inside the data does not end the image. A filtered image is refused: its data length is
/// not in the dictionary, so no end can be proven. Keys are compared after `#xx` decoding. A key and its abbreviation are
/// one key, so a dictionary that gives both is refused.
fn inline_image_end(data: &[u8], bi_end: usize) -> PResult<usize> {
    let mut entries = InlineEntries::default();
    let mut seen: Vec<InlineKey> = Vec::new();
    let mut i = bi_end;
    loop {
        i = skip_space(data, i);
        let word_end = token_end(data, i);
        if &data[i..word_end] == b"ID" {
            i = word_end;
            break;
        }
        if data.get(i) != Some(&b'/') {
            return Err(boundary_error("the inline image dictionary has an unexpected token"));
        }
        let key_end = token_end(data, i + 1);
        let name = decode_name(&data[i + 1..key_end])
            .ok_or_else(|| boundary_error("an inline image dictionary name has a malformed escape"))?;
        let value_start = skip_space(data, key_end);
        let value_end = skip_object(data, value_start)
            .ok_or_else(|| boundary_error("an inline image dictionary value is malformed"))?;
        if let Some(key) = InlineKey::from_name(&name) {
            if key == InlineKey::Filter {
                return Err(filtered_error());
            }
            if seen.contains(&key) {
                return Err(boundary_error("an inline image dictionary key is repeated, or given with its abbreviation"));
            }
            seen.push(key);
            let range = Some((value_start, value_end));
            match key {
                InlineKey::Width => entries.width = range,
                InlineKey::Height => entries.height = range,
                InlineKey::BitsPerComponent => entries.bpc = range,
                InlineKey::ImageMask => entries.image_mask = range,
                InlineKey::ColourSpace => entries.colour_space = range,
                // These do not change the data length.
                InlineKey::Decode | InlineKey::DecodeParms | InlineKey::Interpolate | InlineKey::Filter => {}
            }
        }
        i = value_end;
    }
    if !matches!(data.get(i), Some(&b) if is_pdf_space(b)) {
        return Err(boundary_error("ID is not followed by white space"));
    }
    let length = data_length(data, &entries)
        .ok_or_else(|| boundary_error("the data length is not known from the dictionary"))?;
    // The data starts after the one white-space byte that follows ID. CR LF has two readings: the CR alone is the
    // separator and the LF is the first data byte, or both bytes are the separator. A reading is taken only when it is
    // the single one whose data end is proven. Both, or neither, is refused.
    let after_one = proven_end(data, i + 1, length);
    if data.get(i) == Some(&b'\r') && data.get(i + 1) == Some(&b'\n') {
        return match (after_one, proven_end(data, i + 2, length)) {
            (Some(after), None) | (None, Some(after)) => Ok(after),
            (Some(_), Some(_)) => {
                Err(boundary_error("CR LF after ID: both readings end at an EI token, so the data end is ambiguous"))
            }
            (None, None) => Err(boundary_error("CR LF after ID: no EI token follows the data under either reading")),
        };
    }
    after_one.ok_or_else(|| boundary_error("no EI token follows the number of bytes the dictionary gives"))
}

/// The offset just after the `EI` that follows `length` bytes of data starting at `start`, or `None`. The data bytes are
/// not inspected. White space may separate the data from `EI`, and none is also accepted, because the length from the
/// dictionary is what proves the end. `EI` must be a token of its own: a delimiter, white space or the end follows it.
fn proven_end(data: &[u8], start: usize, length: usize) -> Option<usize> {
    let payload_end = start.checked_add(length).filter(|&end| end <= data.len())?;
    let mut ei = payload_end;
    while ei < data.len() && is_pdf_space(data[ei]) {
        ei += 1;
    }
    if !data[ei..].starts_with(b"EI") {
        return None;
    }
    let after = ei + 2;
    if after < data.len() && !(is_pdf_space(data[after]) || is_delimiter(data[after])) {
        return None;
    }
    Some(after)
}

/// Length in bytes of unfiltered inline image data, from its dictionary. `None` when a value is missing or invalid, or
/// when the colour space or the size cannot be verified.
fn data_length(data: &[u8], entries: &InlineEntries) -> Option<usize> {
    let number = |range: Option<(usize, usize)>| -> Option<u64> {
        let (start, end) = range?;
        let value: i64 = std::str::from_utf8(&data[start..end]).ok()?.parse().ok()?;
        u64::try_from(value).ok()
    };
    let width = number(entries.width).filter(|&v| v > 0)?;
    let height = number(entries.height).filter(|&v| v > 0)?;
    let image_mask = match entries.image_mask {
        None => false,
        Some((start, end)) => match &data[start..end] {
            b"true" => true,
            b"false" => false,
            _ => return None,
        },
    };
    // ISO 32000-1 Table 89: an image mask may omit BitsPerComponent, and then it is 1. A given value must be 1.
    // Any other image needs a legal BitsPerComponent.
    let bpc = match (image_mask, entries.bpc) {
        (true, None) => 1,
        (true, Some(_)) => number(entries.bpc).filter(|&v| v == 1)?,
        (false, _) => number(entries.bpc).filter(|&v| matches!(v, 1 | 2 | 4 | 8 | 16))?,
    };
    let components = if image_mask { 1 } else { colour_components(data, entries.colour_space?)? };
    let stride = width.checked_mul(components)?.checked_mul(bpc)?.div_ceil(8);
    let length = height.checked_mul(stride)?;
    usize::try_from(length).ok()
}

/// Components per pixel of an inline image colour space. Only the standard names, their inline abbreviations, and an
/// Indexed array (one index per pixel) are known. Names are compared after `#xx` decoding. Any other colour space is
/// refused.
fn colour_components(data: &[u8], (start, end): (usize, usize)) -> Option<u64> {
    match *data.get(start)? {
        b'/' => match &*decode_name(&data[start + 1..end])? {
            b"DeviceGray" | b"G" => Some(1),
            b"DeviceRGB" | b"RGB" => Some(3),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        },
        b'[' => indexed_components(data, start, end),
        _ => None,
    }
}

/// Components of `[/Indexed base hival lookup]`, or of the same array with `/I`. The base is a name or an array, hival
/// is an integer from 0 to 255, and lookup is a string. The array must close at `end`, so nothing may follow the lookup.
/// Each pixel is one index, so the count is 1 whatever the base is.
fn indexed_components(data: &[u8], start: usize, end: usize) -> Option<u64> {
    let mut i = skip_space(data, start + 1);
    if data.get(i) != Some(&b'/') {
        return None;
    }
    let name_end = token_end(data, i + 1);
    match &*decode_name(&data[i + 1..name_end])? {
        b"I" | b"Indexed" => {}
        _ => return None,
    }
    i = skip_space(data, name_end);
    if !matches!(data.get(i), Some(&b'/') | Some(&b'[')) {
        return None;
    }
    i = skip_space(data, skip_object(data, i)?);
    let hival_end = token_end(data, i);
    let hival: i64 = std::str::from_utf8(&data[i..hival_end]).ok()?.parse().ok()?;
    if !(0..=255).contains(&hival) {
        return None;
    }
    i = skip_space(data, hival_end);
    i = match *data.get(i)? {
        b'(' => literal_string_end(data, i),
        b'<' if data.get(i + 1) != Some(&b'<') => hex_string_end(data, i),
        _ => return None,
    };
    i = skip_space(data, i);
    (data.get(i) == Some(&b']') && i + 1 == end).then_some(1)
}

/// `i` moved past PDF white space and comments.
fn skip_space(data: &[u8], mut i: usize) -> usize {
    while let Some(&b) = data.get(i) {
        if is_pdf_space(b) {
            i += 1;
        } else if b == b'%' {
            i = comment_end(data, i);
        } else {
            break;
        }
    }
    i
}

/// End of the PDF object that starts at `start`: a name, number, keyword, string, array or dictionary. Arrays and
/// dictionaries are skipped as a whole. A `]` must close a `[`, and a `>>` must close a `<<`, so a crossed pair is
/// malformed. `None` when the object is malformed or is the `ID` keyword.
fn skip_object(data: &[u8], start: usize) -> Option<usize> {
    // Open containers, innermost last: b'[' for an array, b'<' for a dictionary.
    let mut open: Vec<u8> = Vec::new();
    let mut i = start;
    loop {
        i = skip_space(data, i);
        match *data.get(i)? {
            b'(' => i = literal_string_end(data, i),
            b'<' if data.get(i + 1) == Some(&b'<') => {
                open.push(b'<');
                i += 2;
            }
            b'<' => i = hex_string_end(data, i),
            b'[' => {
                open.push(b'[');
                i += 1;
            }
            b'>' if data.get(i + 1) == Some(&b'>') => {
                if open.pop()? != b'<' {
                    return None;
                }
                i += 2;
            }
            b']' => {
                if open.pop()? != b'[' {
                    return None;
                }
                i += 1;
            }
            b'/' => i = token_end(data, i + 1),
            b')' | b'>' | b'{' | b'}' => return None,
            _ => {
                let end = token_end(data, i);
                if &data[i..end] == b"ID" {
                    return None;
                }
                i = end;
            }
        }
        if open.is_empty() {
            return Some(i);
        }
    }
}

fn boundary_error(reason: &str) -> PdfErr {
    PdfErr::invalid(
        "PDF_INLINE_IMAGE_BOUNDARY",
        format!("an inline image's data end cannot be verified ({reason}); the page is not read"),
    )
}

fn filtered_error() -> PdfErr {
    PdfErr::unsupported(
        "PDF_INLINE_IMAGE_FILTER",
        "a filtered inline image has no verifiable data end; the page is not read",
    )
}

/// Result of interpreting one page.
#[derive(Debug)]
pub(crate) struct Interpreted {
    pub glyphs: Vec<RawGlyph>,
    pub warnings: Vec<Warning>,
    /// Image XObject draws and inline images (form XObjects included). Their pixels are never read.
    pub images: u32,
    /// The page's meter, with the units that the page has used. The evidence and annotations of the page are charged to it too.
    pub page: Meter,
    /// Form streams decoded (a form run again is decoded once). Read by the cache tests only.
    #[cfg(test)]
    pub decodes: u64,
}

/// Interprets one physical page's content in document order. Form XObjects are included, under `PAGE_BUDGET` and the request.
pub(crate) fn interpret(doc: &Document, page_id: ObjectId, request: &Request) -> PResult<Interpreted> {
    interpret_with_budget(doc, page_id, PAGE_BUDGET, request)
}

/// `interpret` under the given page limits. Production reads always use `PAGE_BUDGET`; the tests use smaller limits.
pub(crate) fn interpret_with_budget(doc: &Document, page_id: ObjectId, budget: Budget, request: &Request) -> PResult<Interpreted> {
    let page = Meter::page(budget);
    let meters = Meters { page: &page, request: &request.meter };
    let res = page_resources(doc, page_id)?;
    let fonts = Rc::new(font_map(doc, res, meters)?);
    let xobjects = match res {
        Some(r) => sub_dict(doc, r, b"XObject")?,
        None => None,
    };
    let properties = res.and_then(|r| r.get(b"Properties").ok());
    let data = page_content_bytes(doc, page_id, meters)?;
    let operations = decode_operations_within(&data, meters)?;
    let mut interp = Interp {
        doc,
        fonts,
        xobjects,
        properties,
        resolved: HashMap::new(),
        counted_widths: HashSet::new(),
        counted_ignored: HashSet::new(),
        counted_encoding: HashSet::new(),
        counted_type1: HashSet::new(),
        st: GState::initial(),
        stack: Vec::new(),
        tm: Matrix::IDENTITY,
        tlm: Matrix::IDENTITY,
        clips: Vec::new(),
        out: Vec::new(),
        depth: 0,
        warnings: BTreeMap::new(),
        images: 0,
        page,
        request,
        forms: HashMap::new(),
        form_fonts: HashMap::new(),
        marks: Vec::new(),
        hidden_marks: 0,
        mark_base: 0,
        #[cfg(test)]
        decodes: 0,
    };
    interp.run_ops(&operations)?;
    if !interp.marks.is_empty() {
        return Err(unbalanced_marks("marked content is left open at the end of the page"));
    }
    #[cfg(test)]
    let decodes = interp.decodes;
    Ok(Interpreted {
        glyphs: interp.out,
        warnings: interp.warnings.into_iter().collect(),
        images: interp.images,
        page: interp.page,
        #[cfg(test)]
        decodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_product_applies_left_operand_first() {
        let t = Matrix::translate(10.0, 0.0);
        let s = Matrix([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]);
        assert_eq!(t.mul(s).apply(1.0, 0.0), (22.0, 0.0));
        assert_eq!(s.mul(t).apply(1.0, 0.0), (12.0, 0.0));
    }

    #[test]
    fn matrix_determinant_uses_linear_part() {
        let m = Matrix([0.0, 3.0, -2.0, 0.0, 5.0, 7.0]);
        assert_eq!(m.det(), 6.0);
    }

    use std::borrow::Cow;

    fn rewritten(data: &[u8]) -> Vec<u8> {
        with_separators_as_space(data).into_owned()
    }

    fn decode_ops(data: &[u8]) -> PResult<Vec<Operation>> {
        decode_operations(data)
    }

    fn operators(data: &[u8]) -> Vec<String> {
        decode_ops(data).expect("the stream parses").into_iter().map(|op| op.operator).collect()
    }

    fn refusal(data: &[u8]) -> &'static str {
        decode_ops(data).err().expect("the stream is refused").code()
    }

    #[test]
    fn nul_and_form_feed_between_tokens_become_spaces() {
        assert_eq!(rewritten(b"BT\x00/F1\x0c10 Tf\x00"), b"BT /F1 10 Tf ".to_vec());
    }

    #[test]
    fn bytes_inside_literal_and_hex_strings_are_kept() {
        assert_eq!(rewritten(b"(a\x00(b\x0c)c)\x00<4\x0c1>\x00"), b"(a\x00(b\x0c)c) <4\x0c1> ".to_vec());
    }

    #[test]
    fn an_escaped_parenthesis_does_not_end_a_literal_string() {
        assert_eq!(rewritten(b"(x\\)\x00y)\x00"), b"(x\\)\x00y) ".to_vec());
    }

    #[test]
    fn dictionary_delimiters_are_not_read_as_hex_strings() {
        assert_eq!(rewritten(b"<< /A 1 >>\x00"), b"<< /A 1 >> ".to_vec());
    }

    #[test]
    fn a_comment_is_blanked_so_its_open_parenthesis_opens_no_string() {
        assert_eq!(rewritten(b"% (\x00\nBT\x00"), b"    \nBT ".to_vec());
    }

    #[test]
    fn an_unterminated_string_leaves_the_rest_unchanged_and_clean_input_is_not_copied() {
        let open: &[u8] = b"(abc\x00def";
        assert!(matches!(with_separators_as_space(open), Cow::Borrowed(_)));
        let clean: &[u8] = b"BT /F1 10 Tf ET";
        assert!(matches!(with_separators_as_space(clean), Cow::Borrowed(_)));
    }

    #[test]
    fn unfiltered_inline_image_is_bounded_and_the_operators_after_it_are_read() {
        // Two data bytes, NUL and FF, are not separators. The SP before EI is the gap.
        assert_eq!(
            operators(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID \x00\x0c EI BT (Kept) Tj ET"),
            vec!["BI", "BT", "Tj", "ET"]
        );
    }

    #[test]
    fn an_ei_inside_unfiltered_data_does_not_end_the_image() {
        // The 11 data bytes are " EI (F) Tj ". The first " EI " is data, so "(F) Tj" must not become operators.
        assert_eq!(operators(b"BI /W 11 /H 1 /BPC 8 /CS /DeviceGray ID  EI (F) Tj  EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn a_bi_inside_image_data_is_data_not_a_second_image() {
        // The 7 data bytes are "BI /W 9".
        assert_eq!(operators(b"BI /W 7 /H 1 /BPC 8 /CS /DeviceGray ID BI /W 9 EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn a_filtered_inline_image_is_refused_even_when_its_end_looks_clear() {
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /F /AHx ID 00> EI Q"), "PDF_INLINE_IMAGE_FILTER");
    }

    #[test]
    fn a_filtered_inline_image_with_a_fake_ei_cannot_feed_operators() {
        assert_eq!(
            refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /Filter /FlateDecode ID x EI (Fake) Tj EI Q"),
            "PDF_INLINE_IMAGE_FILTER"
        );
    }

    #[test]
    fn data_that_does_not_match_the_dictionary_length_is_refused() {
        // Two bytes are given, but a third byte comes before EI.
        assert_eq!(refusal(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID abc EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
        // Five bytes are given. The fifth byte is the E of EI, so EI does not follow the data.
        assert_eq!(refusal(b"BI /W 5 /H 1 /BPC 8 /CS /DeviceGray ID abc EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
        // No EI at all.
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID a Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn an_unknown_or_missing_colour_space_is_refused() {
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /ICCBased ID 0 EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 ID 0 EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn colour_space_names_and_abbreviations_give_their_component_counts() {
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /RGB ID abc EI Q"), vec!["BI", "Q"]);
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /CMYK ID abcd EI Q"), vec!["BI", "Q"]);
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /G ID a EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn an_indexed_colour_space_has_one_component_per_pixel() {
        assert_eq!(operators(b"BI /W 2 /H 1 /BPC 8 /CS [/I /RGB 1 <000000ffffff>] ID ab EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn an_image_mask_needs_no_colour_space_and_one_bit_per_pixel() {
        assert_eq!(operators(b"BI /IM true /W 8 /H 1 /BPC 1 ID \xff EI Q"), vec!["BI", "Q"]);
        assert_eq!(refusal(b"BI /IM true /W 1 /H 1 /BPC 8 ID \xff EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn bi_inside_a_string_or_a_comment_is_not_an_image() {
        assert_eq!(operators(b"(BI /W 1 ID) Tj % BI /W 1 ID\nBT ET"), vec!["Tj", "BT", "ET"]);
    }

    #[test]
    fn nul_or_ff_right_before_ei_is_the_gap_not_image_data() {
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID \x80\x00EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn nul_after_id_is_the_separator() {
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID\x00\x80\nEI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn id_must_be_followed_by_white_space_and_ei_must_be_a_token() {
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID/x 0 EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID 0 EIQ"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn unknown_keys_with_arrays_and_comments_are_skipped() {
        // The comment contains the word ID, which must not end the dictionary.
        let data: &[u8] = b"BI /Decode [1 0] /W 1 % ID\n/H 1 /BPC 8 /CS /DeviceGray ID a EI Q";
        assert_eq!(operators(data), vec!["BI", "Q"]);
    }

    #[test]
    fn a_repeated_dictionary_key_is_refused() {
        assert_eq!(refusal(b"BI /W 1 /W 2 /H 1 /BPC 8 /CS /DeviceGray ID a EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn bounded_copy_replaces_only_the_image_and_keeps_the_rest() {
        let (bounded, images) =
            bound_inline_images(b"q BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID \x00\x0c EI Q").expect("bounded");
        assert_eq!(images, 1);
        assert_eq!(&*bounded, &b"q BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID 0 EI Q"[..]);
    }

    #[test]
    fn a_bi_operator_that_is_not_the_bounded_image_fails_the_page() {
        // lopdf's fallback returns "BI" with no operand. That must fail the page, not read the data as operators.
        let bare = vec![Operation::new("BI", Vec::new())];
        assert!(check_inline_images(&bare, 1).is_err());
        assert!(check_inline_images(&[], 1).is_err());
        assert!(check_inline_images(&[], 0).is_ok());
    }

    #[test]
    fn an_escaped_filter_key_is_still_a_filter() {
        // Each key decodes to `Filter` or `F`. If the key were skipped, the bytes after `ID` would be read as operators.
        for key in ["/F#69lter", "/#46", "/#46#69#6c#74#65#72", "/#46#69lter"] {
            let data = format!("BI /W 1 /H 1 /BPC 8 /CS /DeviceGray {key} /FlateDecode ID x EI (Fake) Tj EI Q");
            assert_eq!(refusal(data.as_bytes()), "PDF_INLINE_IMAGE_FILTER", "{key}");
        }
    }

    #[test]
    fn escaped_keys_and_name_values_read_as_their_standard_names() {
        // `#49M` is IM, so this is an image mask. 8 pixels are one byte.
        assert_eq!(operators(b"BI /#49M true /W 8 /H 1 ID \xff EI Q"), vec!["BI", "Q"]);
        // `#57` is W, `#48` is H, `#42PC` is BPC, `#43S` is CS, and `/#44eviceGray` is DeviceGray.
        assert_eq!(operators(b"BI /#57 1 /#48 1 /#42PC 8 /#43S /#44eviceGray ID \x00 EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn an_escaped_key_that_repeats_a_plain_key_is_refused() {
        // `/#57` is W, so this repeats W.
        assert_eq!(refusal(b"BI /W 1 /#57 2 /H 1 /BPC 8 /CS /DeviceGray ID a EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn a_malformed_name_escape_is_refused() {
        // An escape needs two hex digits, and a name may not contain NUL.
        for key in ["/F#6", "/#zz", "/#00"] {
            let data = format!("BI /W 1 /H 1 /BPC 8 /CS /DeviceGray {key} /Q ID x EI Q");
            assert_eq!(refusal(data.as_bytes()), "PDF_INLINE_IMAGE_BOUNDARY", "{key}");
        }
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /#4 ID x EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn name_escapes_decode_to_their_bytes() {
        assert_eq!(decode_name(b"F#69lter").map(|name| name.into_owned()), Some(b"Filter".to_vec()));
        assert_eq!(decode_name(b"#2F#2f").map(|name| name.into_owned()), Some(b"//".to_vec()));
        assert!(matches!(decode_name(b"Plain"), Some(Cow::Borrowed(_))));
        assert!(decode_name(b"#4").is_none());
        assert!(decode_name(b"#G0").is_none());
        assert!(decode_name(b"#00").is_none());
        assert!(decode_name(b"#").is_none());
    }

    #[test]
    fn crlf_after_id_is_read_only_when_exactly_one_reading_ends_at_ei() {
        // Both readings end at EI: LF and X, then a space, or X and a space, then EI. The data end is ambiguous.
        assert_eq!(refusal(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID\r\nX EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
        // Only the reading that skips CR LF ends at EI here, so the data is X.
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID\r\nX EI Q"), vec!["BI", "Q"]);
        // Only the reading that skips the CR ends at EI here, so the data is LF and EI follows it directly.
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID\r\nEI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn a_lone_cr_or_lf_after_id_is_the_separator() {
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID\rX EI Q"), vec!["BI", "Q"]);
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID\nX EI Q"), vec!["BI", "Q"]);
    }

    #[test]
    fn a_space_after_the_separator_is_image_data() {
        // The first space is the separator, so the second space is data and the 2-byte image is " X".
        assert_eq!(operators(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID  X EI Q"), vec!["BI", "Q"]);
        // With 1 byte, the second space is the data and X follows it, so the end is not proven.
        assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID  X EI Q"), "PDF_INLINE_IMAGE_BOUNDARY");
    }

    #[test]
    fn crlf_before_ei_is_white_space() {
        assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID X\r\nEI Q"), vec!["BI", "Q"]);
    }
}

#[cfg(test)]
mod image_mask_tests;

#[cfg(test)]
mod inline_boundary_tests;

#[cfg(test)]
mod budget_tests;
