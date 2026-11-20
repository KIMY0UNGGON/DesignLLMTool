//! Encoder: flow model plus the original package to new package bytes.
//!
//! A block whose canonical form equals the original one is copied byte-for-byte. A changed block is
//! regenerated: its original start tag and every unmodelled child are kept, and only the modelled
//! properties and text are written. A new block is generated from the model. Vertical-merge continuation
//! cells are not in the model, so the writer copies them from the original row.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    sha256, AssetStore, DltError, DltResult, ErrorCode, FlowBlock, FlowDocument, Id, Node as ModelNode, PageSetup,
    Paragraph, Section, Table, TableCell, TextRun,
};
use designllm_package::opc::{self, ContentTypes, Relationship};
use designllm_package::xml::{self as bx, Splice};
use designllm_package::Package;
use roxmltree::Node;
use serde_json::Value;

use crate::model::{self, Rec, Records};
use crate::ops::State;
use crate::parse::{self, OriginalPicture, Parsed};
use crate::props::{self, ParaProps, RunProps};
use crate::styles::{self, StyleTable};
use crate::tree::{self, elements, is_ns, is_w, w_attr, Part, StartTag, Tree, NS_A, NS_PIC, NS_R, NS_W, NS_WP};
use crate::units;

pub use designllm_package::opc::{CONTENT_TYPES_PART, REL_IMAGE, REL_OFFICE_DOCUMENT};
pub const REL_STYLES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
pub(crate) const CT_DOCUMENT_MAIN: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
pub(crate) const CT_STYLES: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml";
pub const MAIN_DOCUMENT_PART: &str = "word/document.xml";
pub const STYLES_PART: &str = "word/styles.xml";
pub const DOCUMENT_RELS_PART: &str = "word/_rels/document.xml.rels";
/// Parts this engine may rewrite. Every other part of the original package stays byte-identical.
pub const MUTABLE_PARTS: [&str; 4] = [MAIN_DOCUMENT_PART, STYLES_PART, DOCUMENT_RELS_PART, CONTENT_TYPES_PART];

/// A picture inserted by a change (not present in the original package).
#[derive(Clone, Debug)]
pub struct NewPicture {
    pub rel_id: String,
    pub part: String,
    pub extension: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub docpr_id: u32,
}

/// Native record of a paragraph, read out of [`Rec`].
struct ParaRec {
    path: Option<Vec<usize>>,
    direct: BTreeSet<String>,
    tail: Vec<usize>,
}

fn encode_error(message: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::Internal, message.into())
}

/// Result of a native encode: the package bytes, and the original parts the encode dropped because nothing
/// references them after the change (the media of pictures that the change replaced or removed).
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub released_parts: Vec<String>,
}

/// Encodes `state` into a new package. `base_styles` is the table parsed from the original styles part
/// (without overrides); the merged table is derived from `state.overrides`.
pub fn encode_package(package_bytes: &[u8], state: &State, base_styles: &StyleTable, assets: &dyn AssetStore) -> DltResult<Vec<u8>> {
    Ok(encode_detailed(package_bytes, state, base_styles, assets)?.bytes)
}

/// [`encode_package`] that also reports the original parts it dropped.
pub fn encode_detailed(package_bytes: &[u8], state: &State, base_styles: &StyleTable, assets: &dyn AssetStore) -> DltResult<Encoded> {
    let mut package = crate::guard::open_input_package(package_bytes)?;
    let doc_part = main_document_part(&package)?;
    let doc_bytes = package.part(&doc_part)?.to_vec();
    let rels = read_rels(&package, &doc_part)?;
    let rels_map: BTreeMap<String, String> = rels.iter().map(|r| (r.id.clone(), opc::resolve_target(&doc_part, &r.target).unwrap_or_default())).collect();
    let original = {
        let read = |name: &str| package.part(name).ok().map(|b| b.to_vec());
        parse::parse_document(&doc_bytes, base_styles, &rels_map, &read)?
    };
    let media = plan_media(state, &original, &doc_part, &doc_bytes, &package, assets)?;
    // A model equal to the original keeps the original document part. The body writer rebuilds the body from
    // the model, and the model holds neither XML comments nor processing instructions, so a rewrite would drop them.
    // Node ids are compared out: a fresh parse assigns new ids, so only the canonical content is compared.
    let canon = |document: &FlowDocument| -> Vec<Value> { model::flatten(document).into_iter().map(|f| f.canon).collect() };
    if canon(&state.document) != canon(&original.document) {
        let document = write_document(&doc_bytes, &original, state, &media)?;
        package.set_part(&doc_part, document)?;
    }

    // A picture the change removed or replaced releases its relationship, unless the rewritten body still names
    // it (protected XML may). Its media part is dropped afterwards only when no relationship targets it any more.
    let used = referenced_relationships(package.part(&doc_part)?)?;
    let released = released_pictures(state, &original);
    let dropped: Vec<&OriginalPicture> = released.into_iter().filter(|picture| !used.contains(&picture.rel_id)).collect();
    let dropped_rels: BTreeSet<String> = dropped.iter().map(|picture| picture.rel_id.clone()).collect();
    let dropped_parts: BTreeSet<String> = dropped.iter().map(|picture| picture.part.clone()).collect();

    if !state.overrides.is_empty() {
        let styles_name = styles_part_name(&rels_map);
        if let Some(name) = styles_name {
            let bytes = package.part(&name)?.to_vec();
            let mut merged = base_styles.clone();
            merged.apply_overrides(&state.overrides);
            let keys: BTreeSet<String> = state.overrides.keys().cloned().collect();
            let rewritten = styles::write_style_overrides(&bytes, &merged, &keys)?;
            package.set_part(&name, rewritten)?;
        }
    }

    if !media.is_empty() {
        for picture in media.values() {
            package.set_part(&picture.part, picture.bytes.clone())?;
        }
        // Pictures with the same bytes share one part and one relationship: one entry per relationship id.
        let distinct: BTreeMap<&str, &NewPicture> = media.values().map(|picture| (picture.rel_id.as_str(), picture)).collect();
        let mut updated_rels: Vec<Relationship> = rels.iter().filter(|r| !dropped_rels.contains(&r.id)).cloned().collect();
        for picture in distinct.values() {
            updated_rels.push(Relationship {
                id: picture.rel_id.clone(),
                rel_type: REL_IMAGE.to_string(),
                target: picture.part.trim_start_matches("word/").to_string(),
                external: false,
            });
        }
        package.set_part(DOCUMENT_RELS_PART, opc::rels_to_xml(&updated_rels))?;
        let mut content_types = ContentTypes::parse(package.part(CONTENT_TYPES_PART)?)?;
        for picture in distinct.values() {
            if content_types.content_type_for(&picture.part).is_none() {
                content_types.set_default(&picture.extension, &picture.mime);
            }
        }
        package.set_part(CONTENT_TYPES_PART, content_types.to_xml())?;
    } else if !dropped_rels.is_empty() {
        let rels_bytes = package.part(DOCUMENT_RELS_PART)?.to_vec();
        package.set_part(DOCUMENT_RELS_PART, remove_relationships(&rels_bytes, &dropped_rels)?)?;
    }

    let released_parts = drop_unused_parts(&mut package, dropped_parts)?;
    Ok(Encoded { bytes: package.encode()?, released_parts })
}

/// Name of the main document part, from the package relationships (default `word/document.xml`).
pub fn main_document_part(package: &Package) -> DltResult<String> {
    let rels_bytes = package.part("_rels/.rels")?;
    let rels = opc::parse_rels(rels_bytes)?;
    let target = rels
        .iter()
        .find(|r| r.rel_type == REL_OFFICE_DOCUMENT && !r.external)
        .map(|r| opc::resolve_target("", &r.target))
        .transpose()?
        .unwrap_or_else(|| MAIN_DOCUMENT_PART.to_string());
    if !package.has_part(&target) {
        return Err(DltError::invalid(format!("the package has no main document part {target}")));
    }
    Ok(target)
}

/// Relationships of the document part.
pub fn read_rels(package: &Package, doc_part: &str) -> DltResult<Vec<Relationship>> {
    let name = opc::rels_part_for(doc_part);
    if !package.has_part(&name) {
        return Ok(Vec::new());
    }
    opc::parse_rels(package.part(&name)?)
}

fn styles_part_name(rels: &BTreeMap<String, String>) -> Option<String> {
    rels.values().find(|t| t.ends_with("styles.xml")).cloned().or_else(|| Some(STYLES_PART.to_string()))
}

/// Plans the pictures that the change adds or replaces: relationship ids, part names, and docPr ids. Pictures
/// with the same bytes share one relationship and one part.
fn plan_media(
    state: &State,
    original: &Parsed,
    doc_part: &str,
    doc_bytes: &[u8],
    package: &Package,
    assets: &dyn AssetStore,
) -> DltResult<BTreeMap<Id, NewPicture>> {
    let mut changed_nodes: Vec<&ModelNode> = Vec::new();
    for section in &state.document.sections {
        collect_changed_pictures(&section.blocks, &state.records, original, &mut changed_nodes)?;
    }
    let mut out = BTreeMap::new();
    if changed_nodes.is_empty() {
        return Ok(out);
    }
    let existing_names: BTreeSet<String> = package.names().into_iter().collect();
    let rels = read_rels(package, doc_part)?;
    let mut next_rel = rels
        .iter()
        .filter_map(|r| r.id.strip_prefix("rId").and_then(|n| n.parse::<u64>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let mut next_media = existing_names.iter().filter(|n| n.starts_with("word/media/")).count() as u64 + 1;
    let tree = Tree::parse(doc_bytes)?;
    let mut next_docpr = tree
        .root()
        .descendants()
        .filter(|n| is_ns(*n, NS_WP, "docPr"))
        .filter_map(|n| n.attribute("id").and_then(|v| v.parse::<u32>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let mut by_sha: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    for node in changed_nodes {
        let asset = node.asset.as_ref().ok_or_else(|| encode_error("a new picture has no asset"))?;
        let extension = match asset.mime.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpeg",
            "image/gif" => "gif",
            "image/bmp" => "bmp",
            other => return Err(DltError::unsupported(format!("picture type {other} cannot be written to DOCX"))),
        };
        let (rel_id, part) = match by_sha.get(&asset.sha256) {
            Some((rel, part, _)) => (rel.clone(), part.clone()),
            None => {
                let rel_id = format!("rId{next_rel}");
                next_rel += 1;
                let mut part = format!("word/media/image{next_media}.{extension}");
                while existing_names.contains(&part) {
                    next_media += 1;
                    part = format!("word/media/image{next_media}.{extension}");
                }
                next_media += 1;
                by_sha.insert(asset.sha256.clone(), (rel_id.clone(), part.clone(), extension.to_string()));
                (rel_id, part)
            }
        };
        let bytes = assets.read(asset)?;
        if sha256(&bytes) != asset.sha256 {
            return Err(DltError::new(ErrorCode::IntegrityError, "picture bytes do not match the asset hash"));
        }
        let docpr_id = next_docpr;
        next_docpr += 1;
        out.insert(
            node.id.clone(),
            NewPicture { rel_id, part, extension: extension.to_string(), mime: asset.mime.clone(), bytes, docpr_id },
        );
    }
    Ok(out)
}

/// Pictures that the change adds (no original path) or gives new bytes (an original path, another hash).
fn collect_changed_pictures<'a>(
    blocks: &'a [FlowBlock],
    records: &Records,
    original: &Parsed,
    out: &mut Vec<&'a ModelNode>,
) -> DltResult<()> {
    for block in blocks {
        match block {
            FlowBlock::Image(node) => match records.get(&node.id) {
                Some(Rec::Image { path: None }) => out.push(node),
                Some(Rec::Image { path: Some(path) }) => {
                    let picture = original
                        .pictures
                        .get(path)
                        .ok_or_else(|| encode_error(format!("picture {} has no original picture at its path", node.id)))?;
                    if node.asset.as_ref().map(|a| a.sha256.as_str()) != Some(picture.sha.as_str()) {
                        if let Some(problem) = &picture.replace_problem {
                            let mut error = DltError::new(
                                ErrorCode::UnsafeRoundtrip,
                                format!("picture {} cannot take a new image: {problem}, and the saved picture would keep that setting", node.id),
                            );
                            error.affected_ids = vec![node.id.clone()];
                            return Err(error);
                        }
                        out.push(node);
                    }
                }
                _ => {}
            },
            FlowBlock::Table(t) => {
                for row in &t.rows {
                    for cell in row {
                        collect_changed_pictures(&cell.blocks, records, original, out)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Hash of every picture of the model, keyed by its original path.
fn collect_picture_hashes(blocks: &[FlowBlock], records: &Records, out: &mut BTreeMap<Vec<usize>, String>) {
    for block in blocks {
        match block {
            FlowBlock::Image(node) => {
                if let Some(Rec::Image { path: Some(path) }) = records.get(&node.id) {
                    let sha = node.asset.as_ref().map(|a| a.sha256.clone()).unwrap_or_default();
                    out.insert(path.clone(), sha);
                }
            }
            FlowBlock::Table(t) => {
                for row in &t.rows {
                    for cell in row {
                        collect_picture_hashes(&cell.blocks, records, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Original pictures that the change removed (no picture at their path any more) or replaced (another hash).
fn released_pictures<'a>(state: &State, original: &'a Parsed) -> Vec<&'a OriginalPicture> {
    let mut current: BTreeMap<Vec<usize>, String> = BTreeMap::new();
    for section in &state.document.sections {
        collect_picture_hashes(&section.blocks, &state.records, &mut current);
    }
    original
        .pictures
        .iter()
        .filter(|(path, picture)| match current.get(*path) {
            Some(sha) => sha != &picture.sha,
            None => true,
        })
        .map(|(_, picture)| picture)
        .collect()
}

/// Relationship ids that the body names through an `r:` attribute. Protected XML is included.
fn referenced_relationships(document: &[u8]) -> DltResult<BTreeSet<String>> {
    let tree = Tree::parse(document)?;
    let mut ids = BTreeSet::new();
    for node in tree.root().descendants().filter(|n| n.is_element()) {
        for attribute in node.attributes() {
            if attribute.namespace() == Some(NS_R) {
                ids.insert(attribute.value().to_string());
            }
        }
    }
    Ok(ids)
}

/// Removes the `Relationship` elements with these ids. The other bytes of the part stay as they are.
fn remove_relationships(xml: &[u8], ids: &BTreeSet<String>) -> DltResult<Vec<u8>> {
    let tree = Tree::parse(xml)?;
    let splices: Vec<Splice> = tree
        .root()
        .children()
        .filter(|node| node.is_element() && node.attribute("Id").is_some_and(|id| ids.contains(id)))
        .map(|node| Splice { range: tree.span(node), replacement: Vec::new() })
        .collect();
    bx::apply(xml, splices)
}

/// Every part that some relationship of the package names. Each `.rels` part resolves against its owner.
fn relationship_targets(package: &Package) -> DltResult<BTreeSet<String>> {
    let mut targets = BTreeSet::new();
    for name in package.names() {
        let Some(owner) = opc::owner_for_rels(&name) else { continue };
        for rel in opc::parse_rels(package.part(&name)?)? {
            if !rel.external {
                if let Ok(target) = opc::resolve_target(&owner, &rel.target) {
                    targets.insert(target);
                }
            }
        }
    }
    Ok(targets)
}

/// Removes the candidate parts that no relationship of the package targets, with their content type override.
/// Returns the names of the parts that were removed.
fn drop_unused_parts(package: &mut Package, candidates: BTreeSet<String>) -> DltResult<Vec<String>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let targets = relationship_targets(package)?;
    let mut removed = Vec::new();
    for part in candidates {
        if targets.contains(&part) || !package.has_part(&part) {
            continue;
        }
        package.remove_part(&part)?;
        let mut content_types = ContentTypes::parse(package.part(CONTENT_TYPES_PART)?)?;
        if content_types.overrides.keys().any(|name| name.eq_ignore_ascii_case(&part)) {
            content_types.remove_override(&part);
            package.set_part(CONTENT_TYPES_PART, content_types.to_xml())?;
        }
        removed.push(part);
    }
    Ok(removed)
}

/// Writes the body of a document part. Returns the new document bytes.
fn write_document(doc_bytes: &[u8], original: &Parsed, state: &State, media: &BTreeMap<Id, NewPicture>) -> DltResult<Vec<u8>> {
    let tree = Tree::parse(doc_bytes)?;
    let root = tree.root();
    let body = elements(root)
        .into_iter()
        .find(|n| is_w(*n, "body"))
        .ok_or_else(|| encode_error("w:body is missing"))?;
    let writer = Writer { tree: &tree, original, records: &state.records, media };
    let body_bytes = writer.body(&state.document, body)?;
    let mut splices = vec![Splice { range: tree.span(body), replacement: body_bytes }];
    if let Some(root_splice) = namespace_splice(&tree, root)? {
        splices.push(root_splice);
    }
    bx::apply(doc_bytes, splices)
}

/// Declares the namespaces that generated markup uses, if the root element does not declare them.
fn namespace_splice(tree: &Tree<'_>, root: Node<'_, '_>) -> DltResult<Option<Splice>> {
    let needed = [("w", NS_W), ("r", NS_R), ("wp", NS_WP), ("a", NS_A), ("pic", NS_PIC)];
    let mut additions = String::new();
    for (prefix, uri) in needed {
        match root.lookup_namespace_uri(Some(prefix)) {
            Some(existing) if existing == uri => {}
            Some(_) => {
                return Err(DltError::unsupported(format!("the document binds prefix {prefix} to another namespace")));
            }
            None => additions.push_str(&format!(" xmlns:{prefix}=\"{uri}\"")),
        }
    }
    if additions.is_empty() {
        return Ok(None);
    }
    let span = tree.span(root);
    let start = tree.start_tag(&span)?;
    let raw = tree.raw(&start);
    let mut out = Vec::with_capacity(raw.len() + additions.len());
    let insert_at = if raw.ends_with(b"/>") { raw.len() - 2 } else { raw.len() - 1 };
    out.extend_from_slice(&raw[..insert_at]);
    out.extend_from_slice(additions.as_bytes());
    out.extend_from_slice(&raw[insert_at..]);
    Ok(Some(Splice { range: start, replacement: out }))
}

struct Writer<'a> {
    tree: &'a Tree<'a>,
    original: &'a Parsed,
    records: &'a Records,
    media: &'a BTreeMap<Id, NewPicture>,
}

fn open_head(tree: &Tree<'_>, node: Node<'_, '_>) -> DltResult<Vec<u8>> {
    let span = tree.span(node);
    let start = tree.start_tag(&span)?;
    let raw = tree.raw(&start);
    if raw.ends_with(b"/>") {
        let mut out = raw[..raw.len() - 2].to_vec();
        out.push(b'>');
        Ok(out)
    } else {
        Ok(raw.to_vec())
    }
}

fn text_markup(text: &str) -> DltResult<String> {
    let mut out = String::new();
    for (line_index, line) in text.split('\n').enumerate() {
        if line_index > 0 {
            out.push_str("<w:br/>");
        }
        for (seg_index, segment) in line.split('\t').enumerate() {
            if seg_index > 0 {
                out.push_str("<w:tab/>");
            }
            if !segment.is_empty() {
                out.push_str("<w:t xml:space=\"preserve\">");
                out.push_str(&bx::escape_text(segment)?);
                out.push_str("</w:t>");
            }
        }
    }
    Ok(out)
}

/// Column start of each model cell (HTML-style spans: covered positions have no cell).
fn column_starts(rows: &[Vec<TableCell>], columns: usize) -> Vec<Vec<usize>> {
    let mut occupied = vec![vec![false; columns.max(1) + 64]; rows.len()];
    let mut out = Vec::with_capacity(rows.len());
    for (r, row) in rows.iter().enumerate() {
        let mut c = 0usize;
        let mut starts = Vec::with_capacity(row.len());
        for cell in row {
            while c < occupied[r].len() && occupied[r][c] {
                c += 1;
            }
            starts.push(c);
            let (cs, rs) = (cell.column_span.max(1) as usize, cell.row_span.max(1) as usize);
            for rr in r..(r + rs).min(rows.len()) {
                for cc in c..(c + cs).min(occupied[rr].len()) {
                    occupied[rr][cc] = true;
                }
            }
            c += cs;
        }
        out.push(starts);
    }
    out
}

fn is_continuation(node: Node<'_, '_>) -> bool {
    elements(node)
        .into_iter()
        .find(|c| is_w(*c, "tcPr"))
        .and_then(|pr| elements(pr).into_iter().find(|c| is_w(*c, "vMerge")))
        .map(|m| w_attr(m, "val") != Some("restart"))
        .unwrap_or(false)
}

fn twips_text(emu: i64) -> DltResult<String> {
    let (twips, applied) = units::to_twips(emu)?;
    if applied != emu {
        // The model stores applied values; a mismatch means the model was not quantized.
        return Err(encode_error(format!("length {emu} EMU is not on the twip grid")));
    }
    Ok(twips.to_string())
}

impl<'a> Writer<'a> {
    fn orig(&self, path: &[usize]) -> DltResult<Node<'a, 'a>> {
        tree::node_at(self.tree.root(), path).ok_or_else(|| encode_error(format!("original element {path:?} is missing")))
    }

    fn raw_of(&self, node: Node<'_, '_>) -> Vec<u8> {
        self.tree.raw(&self.tree.span(node)).to_vec()
    }

    fn canon_matches(&self, path: &[usize], now: &Value) -> bool {
        self.original.canon_by_path.get(path).map(|c| c == now).unwrap_or(false)
    }

    fn para_rec(&self, id: &Id) -> DltResult<ParaRec> {
        match self.records.get(id) {
            Some(Rec::Paragraph { path, direct, tail }) => Ok(ParaRec { path: path.clone(), direct: direct.clone(), tail: tail.clone() }),
            _ => Err(encode_error(format!("paragraph {id} has no native record"))),
        }
    }

    fn body(&self, document: &FlowDocument, body: Node<'a, 'a>) -> DltResult<Vec<u8>> {
        let mut content = Vec::new();
        let count = document.sections.len();
        for (index, section) in document.sections.iter().enumerate() {
            let is_last = index + 1 == count;
            let sect_path = if is_last {
                None
            } else {
                let path = self.section_path(section)?;
                if path.is_empty() {
                    return Err(DltError::unsupported("a section that is not the last one must end with a section break paragraph"));
                }
                Some(path)
            };
            let last_block = section.blocks.len().saturating_sub(1);
            if !is_last && section.blocks.is_empty() {
                return Err(encode_error("a section break needs a paragraph to carry it"));
            }
            for (block_index, block) in section.blocks.iter().enumerate() {
                let brk = match (&sect_path, block_index == last_block) {
                    (Some(path), true) => Some((section, path.as_slice())),
                    _ => None,
                };
                content.extend(self.block(block, brk)?);
            }
        }
        if let Some(last) = document.sections.last() {
            let path = self.section_path(last)?;
            let sect_node = if path.is_empty() { None } else { Some(self.orig(&path)?) };
            let unchanged = self.original.section_by_path.get(&path).map(|c| *c == model::canon_section(last)).unwrap_or(false);
            let bytes = match sect_node {
                Some(node) if unchanged => self.raw_of(node),
                other => self.sect_bytes(other, &last.setup)?,
            };
            content.extend(bytes);
        }
        let head = open_head(self.tree, body)?;
        Ok(tree::assemble(&head, &[Part { key: "content".into(), rank: u32::MAX, bytes: content }], "w:body"))
    }

    /// Native path of a section's `sectPr` (empty when the section has none).
    fn section_path(&self, section: &Section) -> DltResult<Vec<usize>> {
        match self.records.get(&section.id) {
            Some(Rec::Section { sect: Some(path) }) => Ok(path.clone()),
            Some(Rec::Section { sect: None }) => Ok(Vec::new()),
            _ => Err(encode_error(format!("section {} has no native record", section.id))),
        }
    }

    fn block(&self, block: &FlowBlock, brk: Option<(&Section, &[usize])>) -> DltResult<Vec<u8>> {
        match block {
            FlowBlock::Paragraph(p) => {
                let rec = self.para_rec(&p.id)?;
                let sect_changed = match brk {
                    Some((section, path)) => self.original.section_by_path.get(path).map(|c| *c != model::canon_section(section)).unwrap_or(true),
                    None => false,
                };
                if let Some(path) = &rec.path {
                    if !sect_changed && self.canon_matches(path, &model::canon_block(block, true)) {
                        return Ok(self.raw_of(self.orig(path)?));
                    }
                }
                self.paragraph(p, &rec, brk)
            }
            FlowBlock::Table(t) => {
                let path = match self.records.get(&t.id) {
                    Some(Rec::Table { path, .. }) => path.clone(),
                    _ => return Err(encode_error(format!("table {} has no native record", t.id))),
                };
                if let Some(path) = &path {
                    if self.canon_matches(path, &model::canon_block(block, true)) {
                        return Ok(self.raw_of(self.orig(path)?));
                    }
                }
                self.table(t, path)
            }
            FlowBlock::Image(node) => {
                let path = match self.records.get(&node.id) {
                    Some(Rec::Image { path }) => path.clone(),
                    _ => return Err(encode_error(format!("picture {} has no native record", node.id))),
                };
                match path {
                    Some(path) => {
                        if self.canon_matches(&path, &model::canon_block(block, true)) {
                            return Ok(self.raw_of(self.orig(&path)?));
                        }
                        self.patched_picture(&path, node)
                    }
                    None => self.new_picture(node),
                }
            }
            FlowBlock::Opaque { id, .. } => match self.records.get(id) {
                Some(Rec::Opaque { path }) => Ok(self.raw_of(self.orig(path)?)),
                _ => Err(encode_error(format!("opaque block {id} has no native record"))),
            },
        }
    }

    fn paragraph(&self, p: &Paragraph, rec: &ParaRec, brk: Option<(&Section, &[usize])>) -> DltResult<Vec<u8>> {
        let orig = match &rec.path {
            Some(path) => Some(self.orig(path)?),
            None => None,
        };
        let children: Vec<Node<'a, 'a>> = orig.map(elements).unwrap_or_default();
        let orig_ppr = children.iter().copied().find(|n| is_w(*n, "pPr"));
        let mut ppr_parts = match orig_ppr {
            Some(node) => tree::parts_of(self.tree, node, props::ppr_rank),
            None => Vec::new(),
        };
        let mut rounding = Vec::new();
        props::apply_para_props(&mut ppr_parts, &rec.direct, &p.format, &ParaProps::parse(orig_ppr), &mut rounding)?;
        let orig_style = orig_ppr
            .and_then(|n| elements(n).into_iter().find(|c| is_w(*c, "pStyle")))
            .and_then(|c| w_attr(c, "val"))
            .map(str::to_string);
        if p.style_ref != orig_style {
            match &p.style_ref {
                Some(style) => {
                    let markup = format!("<w:pStyle w:val=\"{}\"/>", bx::escape_attr(style)?);
                    tree::set_part(&mut ppr_parts, "w:pStyle", 1, markup.into_bytes());
                }
                None => tree::remove_part(&mut ppr_parts, "w:pStyle"),
            }
        }
        if let Some((section, sect_path)) = brk {
            let sect_node = self.orig(sect_path)?;
            let bytes = self.sect_bytes(Some(sect_node), &section.setup)?;
            tree::set_part(&mut ppr_parts, "w:sectPr", 35, bytes);
        }
        let ppr_bytes = match orig_ppr {
            Some(node) => Some(tree::assemble(&open_head(self.tree, node)?, &ppr_parts, "w:pPr")),
            None if !ppr_parts.is_empty() => Some(tree::assemble(b"<w:pPr>", &ppr_parts, "w:pPr")),
            None => None,
        };

        let mut parts: Vec<Part> = Vec::new();
        if let Some(bytes) = ppr_bytes {
            parts.push(Part { key: "w:pPr".into(), rank: 0, bytes });
        }
        for run in &p.runs {
            let record = self.run_record(&run.id)?;
            for &index in &record.lead {
                let node = children.get(index).copied().ok_or_else(|| encode_error("marker index is out of range"))?;
                parts.push(Part { key: "marker".into(), rank: u32::MAX, bytes: self.raw_of(node) });
            }
            parts.push(Part { key: "run".into(), rank: u32::MAX, bytes: self.run(run, &record, &children)? });
        }
        for &index in &rec.tail {
            let node = children.get(index).copied().ok_or_else(|| encode_error("marker index is out of range"))?;
            parts.push(Part { key: "marker".into(), rank: u32::MAX, bytes: self.raw_of(node) });
        }
        let head = match orig {
            Some(node) => open_head(self.tree, node)?,
            None => b"<w:p>".to_vec(),
        };
        Ok(tree::assemble(&head, &parts, "w:p"))
    }

    fn run_record(&self, id: &Id) -> DltResult<RunRec> {
        match self.records.get(id) {
            Some(Rec::Run { origin, atom, direct, lead, .. }) => Ok(RunRec {
                origin: *origin,
                atom: *atom,
                direct: direct.clone(),
                lead: lead.clone(),
            }),
            _ => Err(encode_error(format!("run {id} has no native record"))),
        }
    }

    fn run(&self, run: &TextRun, record: &RunRec, children: &[Node<'a, 'a>]) -> DltResult<Vec<u8>> {
        if run.protected || record.atom {
            let index = record.origin.ok_or_else(|| encode_error("a protected atom has no original element"))?;
            let node = children.get(index).copied().ok_or_else(|| encode_error("protected atom index is out of range"))?;
            return Ok(self.raw_of(node));
        }
        let orig_run = record
            .origin
            .and_then(|i| children.get(i).copied())
            .filter(|n| is_w(*n, "r"));
        let head = match orig_run {
            Some(node) => open_head(self.tree, node)?,
            None => b"<w:r>".to_vec(),
        };
        let orig_rpr = orig_run.and_then(|n| elements(n).into_iter().find(|c| is_w(*c, "rPr")));
        let mut rpr_parts = match orig_rpr {
            Some(node) => tree::parts_of(self.tree, node, props::rpr_rank),
            None => Vec::new(),
        };
        props::apply_run_props(&mut rpr_parts, &record.direct, &run.style, &RunProps::parse(orig_rpr))?;
        let mut parts: Vec<Part> = Vec::new();
        if orig_rpr.is_some() || !rpr_parts.is_empty() {
            let rpr_head = match orig_rpr {
                Some(node) => open_head(self.tree, node)?,
                None => b"<w:rPr>".to_vec(),
            };
            parts.push(Part { key: "w:rPr".into(), rank: 0, bytes: tree::assemble(&rpr_head, &rpr_parts, "w:rPr") });
        }
        parts.push(Part { key: "text".into(), rank: u32::MAX, bytes: text_markup(&run.text)?.into_bytes() });
        Ok(tree::assemble(&head, &parts, "w:r"))
    }

    fn table(&self, t: &Table, path: Option<Vec<usize>>) -> DltResult<Vec<u8>> {
        let starts = column_starts(&t.rows, t.column_widths.len());
        let grid_changed = match &path {
            Some(p) => {
                let orig_table = self.orig(p)?;
                let orig_grid = grid_of(orig_table)?;
                orig_grid != t.column_widths
            }
            None => true,
        };
        let Some(path) = path else {
            return self.new_table(t, &starts);
        };
        let orig_table = self.orig(&path)?;
        let head = open_head(self.tree, orig_table)?;
        let mut parts: Vec<Part> = Vec::new();
        let mut row_index = 0usize;
        for child in elements(orig_table) {
            if is_w(child, "tr") {
                let row = t.rows.get(row_index).ok_or_else(|| encode_error("the model has fewer rows than the original table"))?;
                let bytes = self.row(child, row, &starts[row_index], &t.column_widths, grid_changed)?;
                parts.push(Part { key: "w:tr".into(), rank: u32::MAX, bytes });
                row_index += 1;
            } else if is_w(child, "tblPr") && grid_changed {
                let mut tblpr_parts = tree::parts_of(self.tree, child, props::tblpr_rank);
                let total: i64 = t.column_widths.iter().sum();
                let total_twips = twips_text(total)?;
                let existing = tblpr_parts.iter().position(|p| p.key == "w:tblW");
                match existing {
                    Some(i) => {
                        let mut part = tblpr_parts[i].clone();
                        tree::patch_part_attrs(&mut part, &[("w:w", Some(total_twips.clone())), ("w:type", Some("dxa".into()))])?;
                        tblpr_parts[i] = part;
                    }
                    None => tree::set_part(&mut tblpr_parts, "w:tblW", 7, format!("<w:tblW w:w=\"{total_twips}\" w:type=\"dxa\"/>").into_bytes()),
                }
                parts.push(Part { key: "w:tblPr".into(), rank: 0, bytes: tree::assemble(&open_head(self.tree, child)?, &tblpr_parts, "w:tblPr") });
            } else if is_w(child, "tblGrid") && grid_changed {
                parts.push(Part { key: "w:tblGrid".into(), rank: u32::MAX, bytes: grid_markup(&open_head(self.tree, child)?, &t.column_widths)? });
            } else {
                parts.push(Part { key: "other".into(), rank: u32::MAX, bytes: self.raw_of(child) });
            }
        }
        if row_index != t.rows.len() {
            return Err(encode_error("the model has more rows than the original table"));
        }
        Ok(tree::assemble(&head, &parts, "w:tbl"))
    }

    fn row(&self, tr: Node<'a, 'a>, cells: &[TableCell], starts: &[usize], grid: &[i64], grid_changed: bool) -> DltResult<Vec<u8>> {
        let head = open_head(self.tree, tr)?;
        let mut parts = Vec::new();
        let mut cell_index = 0usize;
        for child in elements(tr) {
            if is_w(child, "tc") {
                if is_continuation(child) {
                    parts.push(Part { key: "tc".into(), rank: u32::MAX, bytes: self.raw_of(child) });
                    continue;
                }
                let cell = cells.get(cell_index).ok_or_else(|| encode_error("the model has fewer cells than the original row"))?;
                let start = *starts.get(cell_index).ok_or_else(|| encode_error("missing cell position"))?;
                parts.push(Part { key: "tc".into(), rank: u32::MAX, bytes: self.cell(child, cell, start, grid, grid_changed)? });
                cell_index += 1;
            } else {
                parts.push(Part { key: "other".into(), rank: u32::MAX, bytes: self.raw_of(child) });
            }
        }
        if cell_index != cells.len() {
            return Err(encode_error("the model has more cells than the original row"));
        }
        Ok(tree::assemble(&head, &parts, "w:tr"))
    }

    fn cell(&self, tc: Node<'a, 'a>, cell: &TableCell, start: usize, grid: &[i64], grid_changed: bool) -> DltResult<Vec<u8>> {
        let head = open_head(self.tree, tc)?;
        let tcpr = elements(tc).into_iter().find(|n| is_w(*n, "tcPr"));
        let mut tcpr_parts = match tcpr {
            Some(node) => tree::parts_of(self.tree, node, props::tcpr_rank),
            None => Vec::new(),
        };
        if grid_changed {
            let span = cell.column_span.max(1) as usize;
            let end = (start + span).min(grid.len());
            let width: i64 = grid[start.min(grid.len())..end].iter().sum();
            let twips = twips_text(width)?;
            let existing = tcpr_parts.iter().position(|p| p.key == "w:tcW");
            match existing {
                Some(i) => {
                    let mut part = tcpr_parts[i].clone();
                    tree::patch_part_attrs(&mut part, &[("w:w", Some(twips.clone())), ("w:type", Some("dxa".into()))])?;
                    tcpr_parts[i] = part;
                }
                None => tree::set_part(&mut tcpr_parts, "w:tcW", 2, format!("<w:tcW w:w=\"{twips}\" w:type=\"dxa\"/>").into_bytes()),
            }
        }
        let mut parts = Vec::new();
        if tcpr.is_some() || !tcpr_parts.is_empty() {
            let tcpr_head = match tcpr {
                Some(node) => open_head(self.tree, node)?,
                None => b"<w:tcPr>".to_vec(),
            };
            parts.push(Part { key: "w:tcPr".into(), rank: 0, bytes: tree::assemble(&tcpr_head, &tcpr_parts, "w:tcPr") });
        }
        for block in &cell.blocks {
            parts.push(Part { key: "block".into(), rank: u32::MAX, bytes: self.block(block, None)? });
        }
        Ok(tree::assemble(&head, &parts, "w:tc"))
    }

    fn new_table(&self, t: &Table, starts: &[Vec<usize>]) -> DltResult<Vec<u8>> {
        let total: i64 = t.column_widths.iter().sum();
        let mut out = String::from("<w:tbl><w:tblPr>");
        out.push_str(&format!("<w:tblW w:w=\"{}\" w:type=\"dxa\"/>", twips_text(total)?));
        if t.style.stroke.len() == 7 {
            let color = units::color_file(&t.style.stroke)?;
            let size = ((t.style.stroke_width * 8 + 6_350) / 12_700).clamp(2, 96);
            out.push_str("<w:tblBorders>");
            for edge in ["top", "left", "bottom", "right", "insideH", "insideV"] {
                out.push_str(&format!("<w:{edge} w:val=\"single\" w:sz=\"{size}\" w:space=\"0\" w:color=\"{color}\"/>"));
            }
            out.push_str("</w:tblBorders>");
        }
        out.push_str("</w:tblPr>");
        out.push_str(std::str::from_utf8(&grid_markup(b"<w:tblGrid>", &t.column_widths)?).map_err(|e| encode_error(e.to_string()))?);
        for (r, row) in t.rows.iter().enumerate() {
            out.push_str("<w:tr>");
            for (c, cell) in row.iter().enumerate() {
                out.push_str("<w:tc><w:tcPr>");
                let start = starts[r][c];
                let span = cell.column_span.max(1) as usize;
                let width: i64 = t.column_widths[start..(start + span).min(t.column_widths.len())].iter().sum();
                out.push_str(&format!("<w:tcW w:w=\"{}\" w:type=\"dxa\"/>", twips_text(width)?));
                if span > 1 {
                    out.push_str(&format!("<w:gridSpan w:val=\"{span}\"/>"));
                }
                out.push_str("</w:tcPr>");
                for block in &cell.blocks {
                    out.push_str(std::str::from_utf8(&self.block(block, None)?).map_err(|e| encode_error(e.to_string()))?);
                }
                out.push_str("</w:tc>");
            }
            out.push_str("</w:tr>");
        }
        out.push_str("</w:tbl>");
        Ok(out.into_bytes())
    }

    fn sect_bytes(&self, orig: Option<Node<'a, 'a>>, setup: &PageSetup) -> DltResult<Vec<u8>> {
        let (w, h) = (twips_text(setup.size.width)?, twips_text(setup.size.height)?);
        let landscape = setup.size.width > setup.size.height;
        let margins = [
            ("w:top", twips_text(setup.margin_top)?),
            ("w:right", twips_text(setup.margin_right)?),
            ("w:bottom", twips_text(setup.margin_bottom)?),
            ("w:left", twips_text(setup.margin_left)?),
        ];
        match orig {
            Some(node) => {
                let mut parts = tree::parts_of(self.tree, node, props::sect_rank);
                match parts.iter().position(|p| p.key == "w:pgSz") {
                    Some(i) => {
                        let mut part = parts[i].clone();
                        tree::patch_part_attrs(
                            &mut part,
                            &[
                                ("w:w", Some(w.clone())),
                                ("w:h", Some(h.clone())),
                                ("w:orient", if landscape { Some("landscape".into()) } else { None }),
                            ],
                        )?;
                        parts[i] = part;
                    }
                    None => tree::set_part(&mut parts, "w:pgSz", 5, format!("<w:pgSz w:w=\"{w}\" w:h=\"{h}\"/>").into_bytes()),
                }
                match parts.iter().position(|p| p.key == "w:pgMar") {
                    Some(i) => {
                        let mut part = parts[i].clone();
                        let edits: Vec<(&str, Option<String>)> = margins.iter().map(|(n, v)| (*n, Some(v.clone()))).collect();
                        tree::patch_part_attrs(&mut part, &edits)?;
                        parts[i] = part;
                    }
                    None => {
                        let markup = format!(
                            "<w:pgMar w:top=\"{}\" w:right=\"{}\" w:bottom=\"{}\" w:left=\"{}\" w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/>",
                            margins[0].1, margins[1].1, margins[2].1, margins[3].1
                        );
                        tree::set_part(&mut parts, "w:pgMar", 6, markup.into_bytes());
                    }
                }
                Ok(tree::assemble(&open_head(self.tree, node)?, &parts, "w:sectPr"))
            }
            None => {
                let orient = if landscape { " w:orient=\"landscape\"" } else { "" };
                Ok(format!(
                    "<w:sectPr><w:pgSz w:w=\"{w}\" w:h=\"{h}\"{orient}/><w:pgMar w:top=\"{}\" w:right=\"{}\" w:bottom=\"{}\" w:left=\"{}\" w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr>",
                    margins[0].1, margins[1].1, margins[2].1, margins[3].1
                )
                .into_bytes())
            }
        }
    }

    /// An existing picture whose extent or image changed. Only `wp:extent`, `a:ext`, and, for a picture that has
    /// new bytes, the `r:embed` of `a:blip` change; the rest of the picture stays as it is.
    fn patched_picture(&self, path: &[usize], node: &ModelNode) -> DltResult<Vec<u8>> {
        let para = self.orig(path)?;
        let span = self.tree.span(para);
        let (cx, cy) = (node.bounds.width.to_string(), node.bounds.height.to_string());
        let new_embed = self.media.get(&node.id).map(|picture| picture.rel_id.as_str());
        let mut splices = Vec::new();
        let mut blip_found = false;
        for descendant in para.descendants() {
            if is_ns(descendant, NS_A, "blip") {
                blip_found = true;
                let Some(rel_id) = new_embed else { continue };
                let prefix = descendant
                    .lookup_prefix(NS_R)
                    .ok_or_else(|| encode_error("the picture's blip has no prefix for the relationship namespace"))?;
                let start = self.tree.start_tag(&self.tree.span(descendant))?;
                let mut tag = StartTag::parse(self.tree.raw(&start))?;
                tag.set(&format!("{prefix}:embed"), rel_id)?;
                splices.push(Splice { range: start, replacement: tag.render().into_bytes() });
                continue;
            }
            let is_extent = is_ns(descendant, NS_WP, "extent");
            let is_xfrm_ext = is_ns(descendant, NS_A, "ext") && descendant.parent().map(|p| is_ns(p, NS_A, "xfrm")).unwrap_or(false);
            if !(is_extent || is_xfrm_ext) {
                continue;
            }
            let start = self.tree.start_tag(&self.tree.span(descendant))?;
            let mut tag = StartTag::parse(self.tree.raw(&start))?;
            tag.set("cx", &cx)?;
            tag.set("cy", &cy)?;
            splices.push(Splice { range: start, replacement: tag.render().into_bytes() });
        }
        if new_embed.is_some() && !blip_found {
            return Err(encode_error(format!("picture {} has no blip to point at its new image", node.id)));
        }
        if splices.is_empty() {
            return Err(encode_error("picture has no extent element"));
        }
        tree::apply_local(self.tree.bytes, &span, splices)
    }

    fn new_picture(&self, node: &ModelNode) -> DltResult<Vec<u8>> {
        let picture = self.media.get(&node.id).ok_or_else(|| encode_error(format!("no media planned for picture {}", node.id)))?;
        let (cx, cy) = (node.bounds.width, node.bounds.height);
        let jc = if node.style.align != "left" && !node.style.align.is_empty() {
            format!("<w:pPr><w:jc w:val=\"{}\"/></w:pPr>", props::align_value(&node.style.align))
        } else {
            String::new()
        };
        let name = picture.part.rsplit('/').next().unwrap_or("picture");
        // The alt text goes to wp:docPr descr. An empty text adds nothing, so pictures without alt text keep their bytes.
        let descr = if node.text.is_empty() { String::new() } else { format!(" descr=\"{}\"", bx::escape_attr(&node.text)?) };
        Ok(format!(
            "<w:p>{jc}<w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:effectExtent l=\"0\" t=\"0\" r=\"0\" b=\"0\"/><wp:docPr id=\"{id}\" name=\"Picture {id}\"{descr}/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect=\"1\"/></wp:cNvGraphicFramePr><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic><pic:nvPicPr><pic:cNvPr id=\"{id}\" name=\"{name}\"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>",
            id = picture.docpr_id,
            rid = picture.rel_id,
            descr = descr,
        )
        .into_bytes())
    }
}

/// A run's native record (copy for the writer).
struct RunRec {
    origin: Option<usize>,
    atom: bool,
    direct: BTreeSet<String>,
    lead: Vec<usize>,
}

/// Grid widths (EMU) of an original table, from its `w:tblGrid`.
fn grid_of(table: Node<'_, '_>) -> DltResult<Vec<i64>> {
    let grid = elements(table).into_iter().find(|n| is_w(*n, "tblGrid"));
    let Some(grid) = grid else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for col in elements(grid) {
        let twips = w_attr(col, "w").and_then(units::parse_int).unwrap_or(0);
        out.push(units::twips_to_emu(twips)?);
    }
    Ok(out)
}

/// Rebuilds a `w:tblGrid` from column widths (EMU values on the twip grid).
fn grid_markup(head: &[u8], widths: &[i64]) -> DltResult<Vec<u8>> {
    let mut parts = Vec::with_capacity(widths.len());
    for width in widths {
        parts.push(Part { key: "w:gridCol".into(), rank: u32::MAX, bytes: format!("<w:gridCol w:w=\"{}\"/>", twips_text(*width)?).into_bytes() });
    }
    Ok(tree::assemble(head, &parts, "w:tblGrid"))
}
