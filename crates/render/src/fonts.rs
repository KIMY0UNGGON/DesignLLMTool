//! Font discovery and resolution. Bundled Pretendard first, then installed system faces.
//!
//! Font programs are loaded once per process into a shared, intentionally leaked cache
//! (`&'static [u8]`) so parsed ttf-parser/rustybuzz faces can be kept without re-parsing.
//! Memory is bounded by the set of distinct font files actually used.
use designllm_contracts::{sha256, DltError, DltResult, ErrorCode, FontFaceRef};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

pub const BUNDLED_FAMILY: &str = "Pretendard";
pub const BUNDLED_REGULAR: &str = "Pretendard-Regular.ttf";
pub const BUNDLED_BOLD: &str = "Pretendard-Bold.ttf";
pub const BUNDLED_LICENSE: &str = "OFL-Pretendard.txt";

/// Windows fallback families tried after the requested family and Pretendard.
pub const SYSTEM_FALLBACKS: &[&str] = &[
    "Malgun Gothic", "Segoe UI", "Segoe UI Symbol", "Segoe UI Emoji", "Microsoft YaHei", "Microsoft JhengHei",
    "Yu Gothic", "SimSun", "Nirmala UI", "Leelawadee UI", "Ebrima", "Gadugi", "Javanese Text", "Myanmar Text",
    "Microsoft Himalaya", "Mongolian Baiti", "Microsoft Yi Baiti", "Segoe UI Historic", "Cambria Math", "Arial",
];

pub(crate) struct Face {
    pub key: String,
    pub family: String,
    pub postscript_name: String,
    pub file_sha256: String,
    pub index: u32,
    pub data: &'static [u8],
    pub ttf: ttf_parser::Face<'static>,
    pub hb: rustybuzz::Face<'static>,
    pub units_per_em: u16,
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    pub cap_height: i16,
    pub bbox: [i16; 4],
    pub weight: u16,
    pub italic: bool,
    /// OS/2 fsType allows embedding (installable, editable or preview & print).
    pub embeddable: bool,
    pub is_cff: bool,
    pub color_glyphs: bool,
    /// Bundled OFL face shipped with the application (embeddable in exported SVG).
    pub bundled: bool,
}

impl Face {
    fn parse(data: &'static [u8], file_sha256: String, index: u32, family_hint: Option<String>) -> DltResult<Face> {
        let ttf = ttf_parser::Face::parse(data, index).map_err(|e| DltError::invalid(format!("Font face {index} cannot be parsed: {e}")))?;
        let hb = rustybuzz::Face::from_slice(data, index).ok_or_else(|| DltError::invalid("Font face cannot be used for shaping"))?;
        let name = |id: u16| -> Option<String> {
            ttf.names().into_iter().filter(|n| n.name_id == id && n.is_unicode()).find_map(|n| {
                if n.language() == ttf_parser::Language::English_UnitedStates { n.to_string() } else { None }
            }).or_else(|| ttf.names().into_iter().filter(|n| n.name_id == id && n.is_unicode()).find_map(|n| n.to_string()))
        };
        let postscript_name = name(ttf_parser::name_id::POST_SCRIPT_NAME).unwrap_or_else(|| "Unknown".into());
        let family = family_hint
            .or_else(|| name(ttf_parser::name_id::TYPOGRAPHIC_FAMILY))
            .or_else(|| name(ttf_parser::name_id::FAMILY))
            .unwrap_or_else(|| postscript_name.clone());
        let mut families = vec![family.clone()];
        for n in name_table_families(&ttf) { if !families.contains(&n) { families.push(n); } }
        register_family_names(&file_sha256, index, families);
        let bb = ttf.global_bounding_box();
        let fs_type = ttf.tables().os2.map(|os2| os2.permissions());
        let tables = ttf.tables();
        // Restricted licenses forbid embedding; CFF2 (variable OpenType) is not subset here.
        let embeddable = !matches!(fs_type, Some(Some(ttf_parser::Permissions::Restricted))) && tables.cff2.is_none();
        Ok(Face {
            key: format!("{}-{}", &file_sha256[..16], index),
            family,
            postscript_name,
            index,
            units_per_em: ttf.units_per_em(),
            ascender: ttf.ascender(),
            descender: ttf.descender(),
            line_gap: ttf.line_gap(),
            cap_height: ttf.capital_height().unwrap_or(ttf.ascender()),
            bbox: [bb.x_min, bb.y_min, bb.x_max, bb.y_max],
            weight: ttf.weight().to_number(),
            italic: ttf.is_italic() || ttf.is_oblique(),
            embeddable,
            is_cff: tables.cff.is_some(),
            color_glyphs: tables.colr.is_some() || tables.sbix.is_some() || tables.cbdt.is_some() || tables.svg.is_some(),
            bundled: false,
            file_sha256,
            data,
            ttf,
            hb,
        })
    }

    pub fn face_ref(&self) -> FontFaceRef {
        FontFaceRef { key: self.key.clone(), file_sha256: self.file_sha256.clone(), face_index: self.index, postscript_name: self.postscript_name.clone(), family: self.family.clone(), variations: BTreeMap::new() }
    }

    /// Every non-ignorable scalar of the grapheme has a glyph in this face.
    pub fn covers(&self, grapheme: &str) -> bool {
        grapheme.chars().filter(|c| !is_default_ignorable(*c)).all(|c| self.ttf.glyph_index(c).is_some())
    }

    pub fn has_char(&self, c: char) -> bool { self.ttf.glyph_index(c).is_some() }
}

/// Family names of every face parsed in this process, by (file sha256, face index). Content-derived,
/// so one registry serves every renderer.
fn family_registry() -> &'static RwLock<HashMap<(String, u32), Vec<String>>> {
    static REGISTRY: OnceLock<RwLock<HashMap<(String, u32), Vec<String>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Typographic (16) and font family (1) names of all Unicode name records, every language.
fn name_table_families(ttf: &ttf_parser::Face<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in ttf.names() {
        if (n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY || n.name_id == ttf_parser::name_id::FAMILY) && n.is_unicode() {
            if let Some(s) = n.to_string() {
                let s = s.trim().to_string();
                if !s.is_empty() && !out.contains(&s) { out.push(s); }
            }
        }
    }
    out
}

fn register_family_names(file_sha256: &str, index: u32, names: Vec<String>) {
    if let Ok(mut r) = family_registry().write() {
        let entry = r.entry((file_sha256.to_string(), index)).or_default();
        for n in names { if !entry.contains(&n) { entry.push(n); } }
    }
}

/// Every family name of a shaped face: its recorded `family` plus, for faces loaded by a
/// [`crate::Renderer`] in this process, all localized family names of its name table (e.g.
/// "Malgun Gothic" and "맑은 고딕", "HCR Batang" and "함초롬바탕") and the names the bundled
/// resolver accepts ("Pretendard Regular"/"Pretendard Bold").
pub fn face_family_names(face: &FontFaceRef) -> Vec<String> {
    let mut names = vec![face.family.clone()];
    if let Some(known) = family_registry().read().ok().and_then(|r| r.get(&(face.file_sha256.clone(), face.face_index)).cloned()) {
        for n in known { if !names.contains(&n) { names.push(n); } }
    }
    names
}

/// Whether a requested family names this face under any of its family names (case-insensitive).
/// A run whose `fonts_used` has no such face was drawn with a substitute; the substitute to report
/// is the face's `family`.
pub fn face_matches_family(face: &FontFaceRef, requested: &str) -> bool {
    let want = requested.trim().to_lowercase();
    !want.is_empty() && face_family_names(face).iter().any(|n| n.trim().to_lowercase() == want)
}

pub(crate) fn is_default_ignorable(c: char) -> bool {
    matches!(c as u32, 0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0000..=0xE0FFF)
}

/// Process-wide font program cache: path -> (leaked bytes, sha256).
fn file_cache() -> &'static Mutex<HashMap<PathBuf, (&'static [u8], String)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (&'static [u8], String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn load_file(path: &Path) -> DltResult<(&'static [u8], String)> {
    let mut cache = file_cache().lock().map_err(|_| DltError::new(ErrorCode::Internal, "font cache lock poisoned"))?;
    if let Some(hit) = cache.get(path) { return Ok(hit.clone()); }
    let bytes = std::fs::read(path).map_err(|e| DltError::new(ErrorCode::Io, format!("Cannot read font {}: {e}", path.display())))?;
    let sha = sha256(&bytes);
    let data: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    cache.insert(path.to_path_buf(), (data, sha.clone()));
    Ok((data, sha))
}

/// Requested family resolution result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Candidate { Loaded(usize), System(fontdb::ID) }

pub(crate) struct FontBook {
    db: Option<fontdb::Database>,
    faces: RwLock<Vec<Arc<Face>>>,
    by_key: RwLock<HashMap<String, usize>>,
    system_loaded: Mutex<HashMap<fontdb::ID, Option<usize>>>,
    resolve_cache: Mutex<HashMap<(String, u16, bool), Option<Candidate>>>,
    fallback_cache: Mutex<HashMap<(u16, bool), Vec<Candidate>>>,
    pub regular: usize,
    pub bold: usize,
    catalog_fingerprint: String,
    families: Vec<String>,
}

impl FontBook {
    pub fn new(font_dir: &Path, system: bool) -> DltResult<Self> {
        let mut faces = Vec::new();
        let mut by_key = HashMap::new();
        let mut bundled_ids = Vec::new();
        verify_bundled_manifest(font_dir)?;
        for file in [BUNDLED_REGULAR, BUNDLED_BOLD] {
            let path = font_dir.join(file);
            if !path.is_file() {
                return Err(DltError::new(ErrorCode::NotFound, format!("Bundled font {} is missing", path.display())));
            }
            let (data, sha) = load_file(&path)?;
            let mut face = Face::parse(data, sha, 0, Some(BUNDLED_FAMILY.to_string()))?;
            face.bundled = true;
            // Names `resolve` accepts for this face.
            let alias = if file == BUNDLED_BOLD { "Pretendard Bold" } else { "Pretendard Regular" };
            register_family_names(&face.file_sha256, 0, vec![alias.to_string()]);
            bundled_ids.push(format!("{}#{}", face.file_sha256, face.postscript_name));
            by_key.insert(face.key.clone(), faces.len());
            faces.push(Arc::new(face));
        }
        let db = if system {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            Some(db)
        } else { None };
        let mut families: BTreeSet<String> = BTreeSet::new();
        families.insert(BUNDLED_FAMILY.to_string());
        let mut catalog: Vec<String> = bundled_ids;
        if let Some(db) = &db {
            for info in db.faces() {
                if let Some((name, _)) = info.families.first() { families.insert(name.clone()); }
                let source = match &info.source { fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => p.display().to_string(), fontdb::Source::Binary(_) => "memory".into() };
                catalog.push(format!("{}|{}|{}|{:?}", source, info.index, info.post_script_name, info.families.iter().map(|f| &f.0).collect::<Vec<_>>()));
            }
        }
        catalog.sort();
        let catalog_fingerprint = sha256(catalog.join("\n").as_bytes());
        Ok(Self {
            db,
            faces: RwLock::new(faces),
            by_key: RwLock::new(by_key),
            system_loaded: Mutex::new(HashMap::new()),
            resolve_cache: Mutex::new(HashMap::new()),
            fallback_cache: Mutex::new(HashMap::new()),
            regular: 0,
            bold: 1,
            catalog_fingerprint,
            families: families.into_iter().collect(),
        })
    }

    pub fn catalog_fingerprint(&self) -> &str { &self.catalog_fingerprint }
    pub fn families(&self) -> Vec<String> { self.families.clone() }

    pub fn face(&self, id: usize) -> Arc<Face> { self.faces.read().expect("font lock")[id].clone() }

    pub fn face_by_key(&self, key: &str) -> Option<Arc<Face>> {
        let id = *self.by_key.read().ok()?.get(key)?;
        Some(self.face(id))
    }

    /// Face referenced by a scene. Loaded faces are found by key; otherwise installed faces with
    /// the same PostScript name are loaded and accepted only if the file hash matches.
    pub fn face_for_ref(&self, face: &FontFaceRef) -> DltResult<Arc<Face>> {
        if let Some(found) = self.face_by_key(&face.key) {
            if found.file_sha256 == face.file_sha256 { return Ok(found); }
        }
        if let Some(db) = &self.db {
            let ids: Vec<fontdb::ID> = db.faces().filter(|info| info.post_script_name == face.postscript_name && info.index == face.face_index).map(|info| info.id).collect();
            for id in ids {
                if let Some(loaded) = self.load_system(id) {
                    let f = self.face(loaded);
                    if f.file_sha256 == face.file_sha256 { return Ok(f); }
                }
            }
        }
        Err(DltError::new(ErrorCode::NotFound, format!("Font '{}' ({}) used by the scene is not available to this renderer", face.postscript_name, face.key)))
    }

    fn register(&self, face: Face) -> usize {
        let mut by_key = self.by_key.write().expect("font lock");
        if let Some(id) = by_key.get(&face.key) { return *id; }
        let mut faces = self.faces.write().expect("font lock");
        let id = faces.len();
        by_key.insert(face.key.clone(), id);
        faces.push(Arc::new(face));
        id
    }

    pub fn load_system(&self, id: fontdb::ID) -> Option<usize> {
        if let Some(hit) = self.system_loaded.lock().ok()?.get(&id) { return *hit; }
        let loaded = (|| {
            let db = self.db.as_ref()?;
            let info = db.face(id)?;
            let path = match &info.source { fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => p.clone(), fontdb::Source::Binary(_) => return None };
            let (data, sha) = load_file(&path).ok()?;
            let hint = info.families.iter().find(|f| f.1 == fontdb::Language::English_UnitedStates).or(info.families.first()).map(|f| f.0.clone());
            let face = Face::parse(data, sha, info.index, hint).ok()?;
            Some(self.register(face))
        })();
        if let Ok(mut map) = self.system_loaded.lock() { map.insert(id, loaded); }
        loaded
    }

    fn query(&self, family: &str, weight: u16, italic: bool) -> Option<fontdb::ID> {
        let db = self.db.as_ref()?;
        let families = [fontdb::Family::Name(family)];
        let query = fontdb::Query {
            families: &families,
            weight: fontdb::Weight(weight),
            stretch: fontdb::Stretch::Normal,
            style: if italic { fontdb::Style::Italic } else { fontdb::Style::Normal },
        };
        db.query(&query)
    }

    /// Requested family as a candidate (bundled first, then installed), or None.
    pub fn resolve(&self, family: &str, weight: u16, italic: bool) -> Option<Candidate> {
        let key = (family.to_lowercase(), weight, italic);
        if let Some(hit) = self.resolve_cache.lock().ok()?.get(&key) { return *hit; }
        let trimmed = family.trim();
        let result = if trimmed.eq_ignore_ascii_case(BUNDLED_FAMILY) || trimmed.eq_ignore_ascii_case("Pretendard Regular") {
            Some(Candidate::Loaded(if weight > 500 { self.bold } else { self.regular }))
        } else if trimmed.eq_ignore_ascii_case("Pretendard Bold") {
            Some(Candidate::Loaded(self.bold))
        } else {
            self.query(trimmed, weight, italic).map(Candidate::System)
        };
        if let Ok(mut cache) = self.resolve_cache.lock() { cache.insert(key, result); }
        result
    }

    /// Fallback candidates after the requested family: bundled Pretendard, then system list.
    pub fn fallbacks(&self, weight: u16, italic: bool) -> Vec<Candidate> {
        if let Some(hit) = self.fallback_cache.lock().ok().and_then(|c| c.get(&(weight, italic)).cloned()) { return hit; }
        let mut out = vec![Candidate::Loaded(if weight > 500 { self.bold } else { self.regular })];
        for name in SYSTEM_FALLBACKS {
            if let Some(id) = self.query(name, weight, italic) {
                let c = Candidate::System(id);
                if !out.contains(&c) { out.push(c); }
            }
        }
        if let Ok(mut cache) = self.fallback_cache.lock() { cache.insert((weight, italic), out.clone()); }
        out
    }

    pub fn load(&self, candidate: Candidate) -> Option<usize> {
        match candidate { Candidate::Loaded(id) => Some(id), Candidate::System(id) => self.load_system(id) }
    }
}

/// Check bundled fonts against assets/fonts/manifest.json when the manifest is present.
fn verify_bundled_manifest(font_dir: &Path) -> DltResult<()> {
    let manifest = font_dir.join("manifest.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else { return Ok(()) };
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| DltError::new(ErrorCode::IntegrityError, format!("Font manifest is not valid JSON: {e}")))?;
    for entry in entries {
        let (Some(file), Some(expected)) = (entry["file"].as_str(), entry["sha256"].as_str()) else { continue };
        if file != BUNDLED_REGULAR && file != BUNDLED_BOLD { continue; }
        let path = font_dir.join(file);
        let (_, actual) = load_file(&path)?;
        if actual != expected {
            return Err(DltError::new(ErrorCode::IntegrityError, format!("Bundled font {file} hash {actual} does not match manifest {expected}")));
        }
    }
    Ok(())
}
