//! Native project files. Canvas and WebCode both use a JSON envelope:
//! `{format, format_version, feature_keys?, artifact, assets}`. `feature_keys` is omitted before v3. The artifact is
//! the full Artifact JSON, so `open(encode(x)) == x`.
//! Referenced image bytes are embedded and verified by sha256 on read, so a file stays portable.
//! Asset bytes are never trusted from the envelope without a matching hash.

use crate::canvas::model::flatten;
use crate::common::{base64_encode, base64_refusal, fail, invalid};
use designllm_contracts::base64::{decode_verified, PayloadError};
use designllm_contracts::native_envelope::{
    check_feature_key_list, check_feature_key_presence, check_feature_keys_used, FeatureKeyError, FeatureUseError,
    NATIVE_MAX_EMBEDDED_BYTES, NATIVE_MAX_FILE_BYTES,
};
use designllm_contracts::{
    sha256, Artifact, ArtifactBody, AssetRef, AssetStore, CanvasDocument, DltError, DltResult, ErrorCode, Interaction, NodeKind,
    Style, StyleField, StylePatch, SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Largest total embedded asset payload (the limit the figure project shares).
pub(crate) const MAX_EMBEDDED_BYTES: u64 = NATIVE_MAX_EMBEDDED_BYTES;
pub(crate) const FORMAT_VERSION: u32 = 1;
/// Format version of files that use a v1.2 value outside the P2 features. Older files keep version 1 and their bytes.
pub(crate) const FORMAT_VERSION_V12: u32 = 2;
/// Format version of files that carry versioned persisted feature keys (P2 and later).
pub(crate) const FORMAT_VERSION_P2: u32 = 3;
pub(crate) const FEATURE_LINE_ENDS: &str = "canvas.line_ends.v1";
pub(crate) const FEATURE_LISTS: &str = "canvas.lists.v1";
pub(crate) const FEATURE_TABLES: &str = "canvas.tables.v1";
pub(crate) const FEATURE_TABLE_SPANS: &str = "canvas.table_spans.v1";
pub(crate) const FEATURE_TABLE_SPAN_TEXT: &str = "canvas.table_span_text.v1";
pub(crate) const FEATURE_TABLE_COLUMN_WIDTHS: &str = "canvas.table_column_widths.v1";
pub(crate) const FEATURE_BOARD_MEMOS: &str = "canvas.board_memos.v1";
pub(crate) const FEATURE_BOARD_TITLES: &str = "canvas.board_titles.v1";
pub(crate) const FEATURE_IMAGE_CROP: &str = "canvas.image_crop.v1";
pub(crate) const FEATURE_LAYOUT_GUIDES: &str = "canvas.layout_guides.v1";
pub(crate) const FEATURE_VIDEO: &str = "canvas.video.v1";
pub(crate) const FEATURE_PAGE_SECTIONS: &str = "canvas.page_sections.v1";
pub(crate) const FEATURE_TEXT_STYLE_EXTENSIONS: &str = "canvas.text_style_extensions.v1";
pub(crate) const FEATURE_TOKENS: &str = "canvas.tokens.v1";
pub(crate) const FEATURE_COMPONENTS: &str = "canvas.components.v1";
/// Repeat groups, show conditions and hidden overrides (D21).
pub(crate) const FEATURE_REPEAT_GROUPS: &str = "canvas.repeat_groups.v1";
pub(crate) const FEATURE_ARTIFACT_DESIGN_SYSTEM: &str = designllm_contracts::design_policy::FEATURE_ARTIFACT_DESIGN_SYSTEM;
pub(crate) const PERSISTED_FEATURE_KEYS: [&str; 17] = [FEATURE_ARTIFACT_DESIGN_SYSTEM, FEATURE_BOARD_MEMOS, FEATURE_BOARD_TITLES, FEATURE_COMPONENTS, FEATURE_IMAGE_CROP, FEATURE_LAYOUT_GUIDES, FEATURE_LINE_ENDS, FEATURE_LISTS, FEATURE_PAGE_SECTIONS, FEATURE_REPEAT_GROUPS, FEATURE_TABLE_COLUMN_WIDTHS, FEATURE_TABLE_SPAN_TEXT, FEATURE_TABLE_SPANS, FEATURE_TABLES, FEATURE_TEXT_STYLE_EXTENSIONS, FEATURE_TOKENS, FEATURE_VIDEO];

#[derive(Debug, Serialize, Deserialize)]
struct EmbeddedAsset {
    sha256: String,
    mime: String,
    bytes: u64,
    data_base64: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    format: String,
    format_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    feature_keys: Vec<String>,
    artifact: Value,
    #[serde(default)]
    assets: Vec<EmbeddedAsset>,
}

/// A decoded native file. Asset bytes are verified against their references.
pub(crate) struct Decoded {
    pub artifact: Artifact,
    pub assets: Vec<(AssetRef, Vec<u8>)>,
}

/// Encodes an artifact and the referenced assets (read from `store`) into a native file.
/// `refs` maps sha256 to the reference. Iteration order is by hash, so output is deterministic.
pub(crate) fn encode(
    format: &str,
    artifact: &Artifact,
    refs: &BTreeMap<String, AssetRef>,
    store: &dyn AssetStore,
) -> DltResult<Vec<u8>> {
    // The file must hold exactly the assets decode requires, so a caller cannot embed more or fewer than the artifact names.
    if *refs != referenced_assets(artifact) {
        return Err(fail(
            ErrorCode::InvalidArgument,
            "NATIVE_ASSET_REFS",
            "the assets to embed are not exactly the assets the artifact references",
        ));
    }
    let mut assets = Vec::with_capacity(refs.len());
    let mut total: u64 = 0;
    for reference in refs.values() {
        let bytes = store.read(reference)?;
        if sha256(&bytes) != reference.sha256 || bytes.len() as u64 != reference.bytes {
            return Err(fail(
                ErrorCode::IntegrityError,
                "NATIVE_ASSET_INTEGRITY",
                format!("asset {} does not match its reference", reference.sha256),
            ));
        }
        total += bytes.len() as u64;
        if total > MAX_EMBEDDED_BYTES {
            return Err(fail(
                ErrorCode::InvalidArgument,
                "NATIVE_ASSETS_TOO_LARGE",
                "referenced assets exceed the embedded size limit",
            ));
        }
        assets.push(EmbeddedAsset {
            sha256: reference.sha256.clone(),
            mime: reference.mime.clone(),
            bytes: reference.bytes,
            data_base64: base64_encode(&bytes),
        });
    }
    let feature_keys = feature_keys_of(artifact);
    let envelope = Envelope {
        format: format.to_string(),
        format_version: format_version_of(artifact),
        feature_keys,
        artifact: serde_json::to_value(artifact).map_err(|e| invalid("NATIVE_JSON", e.to_string()))?,
        assets,
    };
    serde_json::to_vec(&envelope).map_err(|e| invalid("NATIVE_JSON", e.to_string()))
}

/// Decodes a native file and verifies its structure, schema version, and embedded asset hashes.
pub(crate) fn decode(format: &str, bytes: &[u8]) -> DltResult<Decoded> {
    if bytes.len() as u64 > NATIVE_MAX_FILE_BYTES {
        return Err(invalid("NATIVE_TOO_LARGE", "native file exceeds the supported size"));
    }
    let envelope: Envelope = serde_json::from_slice(bytes)
        .map_err(|e| invalid("NATIVE_PARSE", format!("file is not a DesignLLM project: {e}")))?;
    if envelope.format != format {
        return Err(fail(
            ErrorCode::InvalidArgument,
            "NATIVE_FORMAT",
            format!("file format is {} but {format} was expected", envelope.format),
        ));
    }
    if !(FORMAT_VERSION..=FORMAT_VERSION_P2).contains(&envelope.format_version) {
        return Err(fail(
            ErrorCode::SchemaUnsupported,
            "NATIVE_VERSION",
            format!("file format version {} is not supported", envelope.format_version),
        ));
    }
    validate_feature_keys(envelope.format_version, &envelope.feature_keys)?;
    let artifact: Artifact = serde_json::from_value(envelope.artifact)
        .map_err(|e| invalid("NATIVE_ARTIFACT", format!("artifact is malformed: {e}")))?;
    let actual = feature_keys_of(&artifact);
    // Legacy v2 files written before v3 existed may carry line ends and/or text style extensions;
    // re-saving upgrades them to v3. Format v1 and all other persisted features still require v3.
    let allowed_below_v3: &[&str] =
        if envelope.format_version == FORMAT_VERSION_V12 { &[FEATURE_LINE_ENDS, FEATURE_TEXT_STYLE_EXTENSIONS] } else { &[] };
    check_feature_keys_used(&envelope.feature_keys, &actual, envelope.format_version == FORMAT_VERSION_P2, allowed_below_v3)
        .map_err(|error| match error {
            FeatureUseError::NeedsKeysVersion => fail(
                ErrorCode::SchemaUnsupported,
                "NATIVE_FEATURE_VERSION",
                "Canvas persisted features require native format version 3",
            ),
            FeatureUseError::Mismatch => fail(
                ErrorCode::SchemaUnsupported,
                "NATIVE_FEATURE_MISMATCH",
                format!("declared native feature keys {:?} do not match artifact features {:?}", envelope.feature_keys, actual),
            ),
        })?;
    if artifact.schema_version > SCHEMA_VERSION {
        return Err(fail(
            ErrorCode::SchemaUnsupported,
            "SCHEMA_NEWER",
            format!("artifact schema {} is newer than {SCHEMA_VERSION}", artifact.schema_version),
        ));
    }
    // The list holds exactly the assets the artifact references, once each (encode writes exactly those), as the figure
    // project requires. It is checked before any payload is decoded, so nothing is restored from a refused file.
    let referenced = referenced_assets(&artifact);
    let mut listed = std::collections::BTreeSet::new();
    for item in &envelope.assets {
        if !listed.insert(item.sha256.as_str()) {
            return Err(fail(ErrorCode::IntegrityError, "NATIVE_ASSET_DUPLICATE", format!("asset {} is listed twice", item.sha256)));
        }
    }
    if let Some(missing) = referenced.keys().find(|hash| !listed.contains(hash.as_str())) {
        return Err(fail(
            ErrorCode::IntegrityError,
            "NATIVE_ASSET_UNLISTED",
            format!("referenced asset {missing} is not listed in the project"),
        ));
    }
    if let Some(extra) = listed.iter().find(|hash| !referenced.contains_key(**hash)) {
        return Err(fail(
            ErrorCode::IntegrityError,
            "NATIVE_ASSET_UNREFERENCED",
            format!("asset {extra} is listed but the artifact does not reference it"),
        ));
    }

    let mut assets = Vec::with_capacity(envelope.assets.len());
    let mut total: u64 = 0;
    for item in envelope.assets {
        let data = decode_verified(&item.data_base64, item.bytes, &item.sha256).map_err(|error| match error {
            PayloadError::Base64(why) => base64_refusal(why),
            PayloadError::Length { .. } => fail(ErrorCode::IntegrityError, "NATIVE_ASSET_SIZE", "embedded asset size mismatch"),
            PayloadError::Hash => fail(ErrorCode::IntegrityError, "NATIVE_ASSET_HASH", "embedded asset hash mismatch"),
        })?;
        total += data.len() as u64;
        if total > MAX_EMBEDDED_BYTES {
            return Err(invalid("NATIVE_ASSETS_TOO_LARGE", "embedded assets exceed the size limit"));
        }
        assets.push((AssetRef { sha256: item.sha256, mime: item.mime, bytes: item.bytes }, data));
    }
    Ok(Decoded { artifact, assets })
}

/// The assets a native file embeds: those the nodes of a canvas reference. A web project references none.
fn referenced_assets(artifact: &Artifact) -> BTreeMap<String, AssetRef> {
    match &artifact.body {
        ArtifactBody::Canvas(doc) => crate::canvas::asset_refs(doc),
        _ => BTreeMap::new(),
    }
}

/// Format version a file is written with: 3 for persisted feature keys, 2 for other v1.2 values, 1 otherwise.
pub(crate) fn format_version_of(artifact: &Artifact) -> u32 {
    if !feature_keys_of(artifact).is_empty() {
        return FORMAT_VERSION_P2;
    }
    match &artifact.body {
        ArtifactBody::Canvas(doc) if uses_v12(doc) => FORMAT_VERSION_V12,
        _ => FORMAT_VERSION,
    }
}

pub(crate) fn feature_keys_of(artifact: &Artifact) -> Vec<String> {
    let ArtifactBody::Canvas(doc) = &artifact.body else { return Vec::new() };
    let mut keys: Vec<String> = feature_keys_for_document(doc).into_iter().map(str::to_string).collect();
    if artifact.design_system.is_some() || artifact.publishes_design_system {
        keys.push(FEATURE_ARTIFACT_DESIGN_SYSTEM.to_string());
    }
    keys.sort();
    keys
}

pub(crate) fn feature_keys_for_document(doc: &CanvasDocument) -> Vec<&'static str> {
    document_feature_flags(doc).into_iter().filter_map(|(key, present)| present.then_some(key)).collect()
}

/// Each document-level persisted key except the design-system one, with whether the document uses it. The keys are written
/// here once; `PERSISTED_FEATURE_KEYS` is the declared list, and a test keeps the two in the same order.
fn document_feature_flags(doc: &CanvasDocument) -> [(&'static str, bool); 16] {
    let board_memos = !doc.board_memos.is_empty();
    let board_titles = doc.board_memos.iter().any(|memo| memo.kind == designllm_contracts::CanvasBoardMemoKind::Title);
    let layout_guides = !doc.layout_guides.is_empty();
    let page_sections = !doc.page_sections.is_empty();
    let mut image_crop = false;
    let mut videos = false;
    let mut line_ends = false;
    let mut lists = false;
    let mut tables = false;
    let mut table_spans = false;
    let mut table_span_text = false;
    let mut table_column_widths = false;
    let mut text_style_extensions = false;
    let mut components = doc.pages.iter().any(|page| page.role.is_some());
    let mut repeat_groups = false;
    let tokens = !doc.tokens.is_empty() || !doc.bindings.is_empty() || doc.token_mode.is_some();
    for style in doc.styles.values() {
        line_ends |= style.stroke_start.is_some() || style.stroke_end.is_some();
        text_style_extensions |= has_text_style_extension(style);
    }
    for node in doc.pages.iter().flat_map(|page| flatten(&page.nodes)) {
        line_ends |= node.style.stroke_start.is_some() || node.style.stroke_end.is_some();
        image_crop |= node.ext.image_crop_pixel_rect.is_some();
        videos |= node.kind == NodeKind::Video || node.ext.video_poster_asset.is_some();
        lists |= node.ext.list.is_some();
        tables |= node.ext.table.is_some();
        table_spans |= node.ext.table.as_ref().is_some_and(|table| !table.cell_spans.is_empty());
        if let Some(table) = &node.ext.table {
            if let Ok(covered) = crate::canvas::model::covered_table_cell_ids(table) {
                table_span_text |= node.children.iter().any(|cell| !cell.text.is_empty() && covered.contains(cell.id.as_str()));
            }
        }
        table_column_widths |= node.ext.table.as_ref().is_some_and(|table| table.column_widths.is_some());
        components |= node.ext.instance.is_some();
        repeat_groups |= crate::canvas::repeat::uses_repeat_groups(node);
        text_style_extensions |= has_text_style_extension(&node.style);
        if let Some(instance) = &node.ext.instance {
            for over in instance.overrides.values() {
                if let Some(patch) = &over.style {
                    line_ends |= patch_has_field(patch, StyleField::StrokeStart) || patch_has_field(patch, StyleField::StrokeEnd);
                    text_style_extensions |= [StyleField::FontWeight, StyleField::Underline, StyleField::Strikethrough]
                        .into_iter().any(|field| patch_has_field(patch, field));
                }
            }
        }
        for patch in node.variants.values() {
            line_ends |= patch_has_field(patch, StyleField::StrokeStart) || patch_has_field(patch, StyleField::StrokeEnd);
            text_style_extensions |= [StyleField::FontWeight, StyleField::Underline, StyleField::Strikethrough]
                .into_iter()
                .any(|field| patch_has_field(patch, field));
        }
    }
    [
        (FEATURE_BOARD_MEMOS, board_memos),
        (FEATURE_BOARD_TITLES, board_titles),
        (FEATURE_COMPONENTS, components),
        (FEATURE_IMAGE_CROP, image_crop),
        (FEATURE_LAYOUT_GUIDES, layout_guides),
        (FEATURE_LINE_ENDS, line_ends),
        (FEATURE_LISTS, lists),
        (FEATURE_PAGE_SECTIONS, page_sections),
        (FEATURE_REPEAT_GROUPS, repeat_groups),
        (FEATURE_TABLE_COLUMN_WIDTHS, table_column_widths),
        (FEATURE_TABLE_SPAN_TEXT, table_span_text),
        (FEATURE_TABLE_SPANS, table_spans),
        (FEATURE_TABLES, tables),
        (FEATURE_TEXT_STYLE_EXTENSIONS, text_style_extensions),
        (FEATURE_TOKENS, tokens),
        (FEATURE_VIDEO, videos),
    ]
}

fn has_text_style_extension(style: &Style) -> bool {
    style.font_weight.is_some() || style.underline.is_some() || style.strikethrough.is_some()
}

fn patch_has_field(patch: &StylePatch, field: StyleField) -> bool {
    patch.set_fields().contains(&field) || patch.clear.contains(&field)
}

/// The declared keys. Serde reads an absent list as empty, so an empty list counts as absent: below v3 it is accepted, and
/// on v3 it is refused as missing.
fn validate_feature_keys(format_version: u32, keys: &[String]) -> DltResult<()> {
    let keys_version = format_version == FORMAT_VERSION_P2;
    check_feature_key_presence(!keys.is_empty(), keys_version).map_err(feature_key_refusal)?;
    if keys_version {
        check_feature_key_list(keys, &PERSISTED_FEATURE_KEYS).map_err(feature_key_refusal)?;
    }
    Ok(())
}

fn feature_key_refusal(error: FeatureKeyError<'_>) -> DltError {
    let (diagnostic, message) = match error {
        FeatureKeyError::NotAllowed => ("NATIVE_FEATURE_KEYS", "feature_keys are only valid on native format version 3".to_owned()),
        FeatureKeyError::Required => ("NATIVE_FEATURE_KEYS", "native format version 3 requires feature_keys".to_owned()),
        FeatureKeyError::Duplicate(key) => ("NATIVE_FEATURE_KEYS", format!("duplicate native feature key {key:?}")),
        FeatureKeyError::Unsorted => ("NATIVE_FEATURE_KEYS", "native feature_keys must be sorted".to_owned()),
        FeatureKeyError::Unsupported(key) => ("NATIVE_FEATURE_KEY_UNSUPPORTED", format!("native feature key {key:?} is not supported")),
    };
    fail(ErrorCode::SchemaUnsupported, diagnostic, message)
}

/// True when the document carries a contract v1.2 value: a page name, a node extension, a v1.2 style or layout field,
/// a path, an OpenUrl or Back interaction, a v1.2 variant field or clear, or a named style with a v1.2 field.
pub(crate) fn uses_v12(doc: &CanvasDocument) -> bool {
    let patch_uses_v12 = |patch: &StylePatch| !patch.clear.is_empty() || patch.set_fields().iter().any(|f| f.is_v12());
    if doc.pages.iter().any(|page| page.name.is_some()) {
        return true;
    }
    if doc.styles.values().any(|style| !style.non_default_v12_fields().is_empty()) {
        return true;
    }
    doc.pages.iter().flat_map(|page| flatten(&page.nodes)).any(|node| {
        node.kind == NodeKind::Path
            || !node.style.non_default_v12_fields().is_empty()
            || !node.ext.set_fields().is_empty()
            || node.layout.as_ref().is_some_and(|spec| !spec.v12_fields().is_empty())
            || node.interactions.iter().any(|i| matches!(i, Interaction::OpenUrl { .. } | Interaction::Back))
            || node.variants.values().any(patch_uses_v12)
    })
}

/// Stores embedded assets in the staging store and checks that each returns the declared reference.
pub(crate) fn restore_assets(decoded: &[(AssetRef, Vec<u8>)], store: &dyn AssetStore) -> DltResult<()> {
    for (reference, data) in decoded {
        let stored = store.put(data, &reference.mime)?;
        if stored.sha256 != reference.sha256 || stored.bytes != reference.bytes {
            return Err(fail(ErrorCode::IntegrityError, "NATIVE_ASSET_STORE", "store returned a different asset"));
        }
    }
    Ok(())
}

/// Equality of two artifacts by their JSON form (the native envelope and the public model).
pub(crate) fn same_artifact(a: &Artifact, b: &Artifact) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod table_span_feature_tests {
    use super::*;
    use designllm_contracts::{ArtifactKind, CanvasBoardMemo, CanvasBoardMemoKind, CanvasPageSection, CanvasTable, CanvasTableRow, CanvasTableSpan, DltError, Node, NodeExt, NodeKind, Page, PixelRect, Rect, Size, Style};

    struct EmptyAssets;
    impl AssetStore for EmptyAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("not used")) }
        fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("not used")) }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
    }

    struct CropAsset { reference: AssetRef, bytes: Vec<u8> }
    impl AssetStore for CropAsset {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("not used")) }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            if asset.sha256 == self.reference.sha256 { Ok(self.bytes.clone()) } else { Err(DltError::new(ErrorCode::NotFound, asset.sha256.clone())) }
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            if hash == self.reference.sha256 { Ok(self.reference.clone()) } else { Err(DltError::new(ErrorCode::NotFound, hash.to_owned())) }
        }
    }

    struct VideoAssets(BTreeMap<String, (AssetRef, Vec<u8>)>);
    impl AssetStore for VideoAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("not used")) }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            self.0.get(&asset.sha256).filter(|(reference, _)| reference == asset).map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, asset.sha256.clone()))
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            self.0.get(hash).map(|(reference, _)| reference.clone()).ok_or_else(|| DltError::new(ErrorCode::NotFound, hash.to_owned()))
        }
    }

    fn artifact(with_spans: bool) -> Artifact {
        let cell = |id: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 100, height: 36 },
            text: String::new(), style: Style::default(), asset: None, children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt::default(),
        };
        let mut table = CanvasTable {
            rows: vec![CanvasTableRow { id: "row".into(), cell_ids: vec!["c1".into(), "c2".into()] }],
            header_row_id: None, column_widths: None, cell_spans: Default::default(),
        };
        if with_spans {
            table.cell_spans.insert("c1".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        }
        Artifact::new(ArtifactKind::Canvas, "table", crate::CANVAS_ENGINE_ID, ArtifactBody::Canvas(CanvasDocument {
            pages: vec![Page {
                nodes: vec![Node {
                    id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 200, height: 36 },
                    text: String::new(), style: Style::default(), asset: None, children: vec![cell("c1"), cell("c2")],
                    layout: None, interactions: Vec::new(), variants: Default::default(),
                    ext: NodeExt { table: Some(table), ..Default::default() },
                }],
                ..Page::new("page", Size::slide())
            }],
            ..Default::default()
        }))
    }

    fn crop_artifact(crop: Option<PixelRect>) -> (Artifact, CropAsset) {
        let bytes = designllm_render::png_from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).expect("small source PNG");
        let reference = AssetRef { sha256: designllm_contracts::sha256(&bytes), mime: "image/png".into(), bytes: bytes.len() as u64 };
        let asset_store = CropAsset { reference: reference.clone(), bytes };
        let image = Node {
            id: "image".into(), kind: NodeKind::Image, bounds: Rect { x: 0, y: 0, width: 200, height: 100 },
            text: String::new(), style: Style::default(), asset: Some(reference), children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt { image_crop_pixel_rect: crop, ..Default::default() },
        };
        let artifact = Artifact::new(ArtifactKind::Canvas, "image", crate::CANVAS_ENGINE_ID, ArtifactBody::Canvas(CanvasDocument {
            pages: vec![Page { nodes: vec![image], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        }));
        (artifact, asset_store)
    }

    #[test]
    fn declared_feature_keys_are_sorted_and_the_document_flags_name_each_of_them() {
        assert!(PERSISTED_FEATURE_KEYS.windows(2).all(|pair| pair[0] < pair[1]), "PERSISTED_FEATURE_KEYS is sorted and unique");
        let flagged: Vec<&str> = document_feature_flags(&CanvasDocument::default()).iter().map(|(key, _)| *key).collect();
        let declared: Vec<&str> = PERSISTED_FEATURE_KEYS.iter().copied().filter(|key| *key != FEATURE_ARTIFACT_DESIGN_SYSTEM).collect();
        assert_eq!(flagged, declared, "a key added to one list must be added to the other");
    }

    #[test]
    fn encode_refuses_assets_other_than_the_ones_the_artifact_references() {
        let (with_image, assets) = crop_artifact(None);
        let extra = AssetRef { sha256: sha256(b"other"), mime: "image/png".into(), bytes: 5 };
        let too_many = BTreeMap::from([
            (assets.reference.sha256.clone(), assets.reference.clone()),
            (extra.sha256.clone(), extra),
        ]);
        for refs in [BTreeMap::new(), too_many] {
            let err = encode(crate::canvas::CANVAS_FORMAT, &with_image, &refs, &assets).expect_err("refs differ from the artifact");
            assert_eq!((err.code, err.diagnostics[0].code.as_str()), (ErrorCode::InvalidArgument, "NATIVE_ASSET_REFS"));
        }
        let exact = BTreeMap::from([(assets.reference.sha256.clone(), assets.reference.clone())]);
        assert!(encode(crate::canvas::CANVAS_FORMAT, &with_image, &exact, &assets).is_ok());
    }

    #[test]
    fn table_span_persisted_feature_is_only_written_when_the_map_is_nonempty() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_TABLE_SPANS));
        assert!(!feature_keys_of(&artifact(false)).contains(&FEATURE_TABLE_SPANS.to_owned()));
        assert!(feature_keys_of(&artifact(true)).contains(&FEATURE_TABLE_SPANS.to_owned()));
    }

    #[test]
    fn table_span_native_round_trip_keeps_metadata_and_feature_key() {
        let artifact = artifact(true);
        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &BTreeMap::new(), &EmptyAssets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        let span_map = document.pages[0].nodes[0].ext.table.as_ref().unwrap().cell_spans.clone();
        assert_eq!(span_map, [
            ("c1".to_owned(), CanvasTableSpan { row_span: 1, column_span: 2 }),
        ].into_iter().collect());
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_TABLE_SPANS.to_owned()));
    }

    #[test]
    fn canvas_image_crop_uses_a_separate_native_feature_key_and_round_trips() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_IMAGE_CROP));
        let (without_crop, _assets) = crop_artifact(None);
        assert!(!feature_keys_of(&without_crop).contains(&FEATURE_IMAGE_CROP.to_owned()));

        let (with_crop, assets) = crop_artifact(Some(PixelRect { x: 1, y: 0, width: 1, height: 1 }));
        assert!(feature_keys_of(&with_crop).contains(&FEATURE_IMAGE_CROP.to_owned()));
        let references = BTreeMap::from([(assets.reference.sha256.clone(), assets.reference.clone())]);
        let bytes = encode(crate::canvas::CANVAS_FORMAT, &with_crop, &references, &assets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.pages[0].nodes[0].ext.image_crop_pixel_rect, Some(PixelRect { x: 1, y: 0, width: 1, height: 1 }));
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_IMAGE_CROP.to_owned()));
    }

    #[test]
    fn video_native_round_trip_keeps_source_poster_assets_and_feature_key() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_VIDEO));
        let mut video_bytes = Vec::new();
        video_bytes.extend_from_slice(&16u32.to_be_bytes());
        video_bytes.extend_from_slice(b"ftypisom");
        video_bytes.extend_from_slice(&0u32.to_be_bytes());
        let poster_bytes = designllm_render::png_from_rgba(1, 1, &[5, 6, 7, 255]).expect("poster PNG");
        let source = AssetRef { sha256: sha256(&video_bytes), mime: "video/mp4".into(), bytes: video_bytes.len() as u64 };
        let poster = AssetRef { sha256: sha256(&poster_bytes), mime: "image/png".into(), bytes: poster_bytes.len() as u64 };
        let assets = VideoAssets([
            (source.sha256.clone(), (source.clone(), video_bytes)),
            (poster.sha256.clone(), (poster.clone(), poster_bytes)),
        ].into_iter().collect());
        let video = Node {
            id: "video".into(), kind: NodeKind::Video, bounds: Rect { x: 0, y: 0, width: 400, height: 300 },
            text: String::new(), style: Style::default(), asset: Some(source.clone()), children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt { video_poster_asset: Some(poster.clone()), ..Default::default() },
        };
        let artifact = Artifact::new(ArtifactKind::Canvas, "video", crate::CANVAS_ENGINE_ID, ArtifactBody::Canvas(CanvasDocument {
            pages: vec![Page { nodes: vec![video], ..Page::new("page", Size::slide()) }],
            ..Default::default()
        }));
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas body") };
        let references = crate::canvas::asset_refs(document);
        assert_eq!(references.len(), 2, "both source video and poster are required assets");
        assert!(feature_keys_of(&artifact).contains(&FEATURE_VIDEO.to_owned()));

        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &references, &assets).expect("native video encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native video decode");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.pages[0].nodes[0].asset.as_ref(), Some(&source));
        assert_eq!(document.pages[0].nodes[0].ext.video_poster_asset.as_ref(), Some(&poster));
        assert_eq!(decoded.assets.len(), 2, "source and poster bytes stay embedded");
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_VIDEO.to_owned()));
    }

    #[test]
    fn populated_covered_text_uses_a_separate_persisted_feature_key() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_TABLE_SPAN_TEXT));
        let mut artifact = artifact(true);
        let ArtifactBody::Canvas(document) = &mut artifact.body else { panic!("Canvas body") };
        document.pages[0].nodes[0].children[1].text = "covered text".into();
        let keys = feature_keys_of(&artifact);
        assert!(keys.contains(&FEATURE_TABLE_SPANS.to_owned()));
        assert!(keys.contains(&FEATURE_TABLE_SPAN_TEXT.to_owned()));

        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &BTreeMap::new(), &EmptyAssets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("current native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.pages[0].nodes[0].children[1].text, "covered text");
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_TABLE_SPAN_TEXT.to_owned()));

        if let ArtifactBody::Canvas(document) = &mut artifact.body {
            document.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans.clear();
        }
        assert!(!feature_keys_of(&artifact).contains(&FEATURE_TABLE_SPAN_TEXT.to_owned()), "unmerge removes the current populated-span feature");
    }

    #[test]
    fn board_memo_feature_is_omitted_when_empty_and_round_trips_when_present() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_BOARD_MEMOS));
        let mut artifact = artifact(false);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas body") };
        assert!(document.board_memos.is_empty());
        assert!(!feature_keys_of(&artifact).contains(&FEATURE_BOARD_MEMOS.to_owned()));
        assert!(!feature_keys_of(&artifact).contains(&FEATURE_BOARD_TITLES.to_owned()));
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(),
            page_id: "page".into(),
            bounds: Rect { x: -10, y: 20, width: 300, height: 200 },
            text: "outside the page frame".into(),
            color: "#FFF2CC".into(),
        };
        if let ArtifactBody::Canvas(document) = &mut artifact.body {
            document.board_memos.push(memo);
        }
        assert!(feature_keys_of(&artifact).contains(&FEATURE_BOARD_MEMOS.to_owned()));
        assert!(!feature_keys_of(&artifact).contains(&FEATURE_BOARD_TITLES.to_owned()));

        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &BTreeMap::new(), &EmptyAssets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.board_memos.len(), 1);
        assert_eq!(document.board_memos[0].bounds.x, -10);
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_BOARD_MEMOS.to_owned()));
    }

    #[test]
    fn board_title_feature_is_declared_separately_from_legacy_memos() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_BOARD_TITLES));
        let mut artifact = artifact(false);
        let ArtifactBody::Canvas(document) = &mut artifact.body else { panic!("Canvas body") };
        document.board_memos.push(CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Title,
            id: "title-1".into(),
            page_id: "page".into(),
            bounds: Rect { x: 0, y: -50_000, width: 300_000, height: 40_000 },
            text: "Board title".into(),
            color: "#223344".into(),
        });
        let keys = feature_keys_of(&artifact);
        assert!(keys.contains(&FEATURE_BOARD_MEMOS.to_owned()));
        assert!(keys.contains(&FEATURE_BOARD_TITLES.to_owned()));

        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &BTreeMap::new(), &EmptyAssets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.board_memos[0].kind, CanvasBoardMemoKind::Title);
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_BOARD_TITLES.to_owned()));
    }

    #[test]
    fn page_section_feature_is_omitted_when_empty_and_round_trips_when_present() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_PAGE_SECTIONS));
        let mut artifact = artifact(false);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas body") };
        assert!(document.page_sections.is_empty());
        assert!(!feature_keys_of(&artifact).contains(&FEATURE_PAGE_SECTIONS.to_owned()));
        if let ArtifactBody::Canvas(document) = &mut artifact.body {
            document.page_sections.push(CanvasPageSection { id: "section-1".into(), name: "Part 1".into(), start_page_id: "page".into() });
        }
        assert!(feature_keys_of(&artifact).contains(&FEATURE_PAGE_SECTIONS.to_owned()));

        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &BTreeMap::new(), &EmptyAssets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.page_sections, [CanvasPageSection { id: "section-1".into(), name: "Part 1".into(), start_page_id: "page".into() }]);
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_PAGE_SECTIONS.to_owned()));
    }

    #[test]
    fn page_layout_guide_feature_is_omitted_when_empty_and_round_trips_when_present() {
        assert!(PERSISTED_FEATURE_KEYS.contains(&FEATURE_LAYOUT_GUIDES));
        let mut artifact = artifact(false);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas body") };
        assert!(document.layout_guides.is_empty());
        assert!(!feature_keys_of(&artifact).contains(&FEATURE_LAYOUT_GUIDES.to_owned()));
        let guides = designllm_contracts::CanvasPageGuides {
            columns: Some(designllm_contracts::CanvasAxisGuides {
                count: 3, spacing_emu: 20_000, leading_margin_emu: 100_000, trailing_margin_emu: 100_000,
            }),
            rows: None,
        };
        if let ArtifactBody::Canvas(document) = &mut artifact.body {
            document.layout_guides.insert("page".into(), guides.clone());
        }
        assert!(feature_keys_of(&artifact).contains(&FEATURE_LAYOUT_GUIDES.to_owned()));

        let bytes = encode(crate::canvas::CANVAS_FORMAT, &artifact, &BTreeMap::new(), &EmptyAssets).expect("native encode");
        let decoded = decode(crate::canvas::CANVAS_FORMAT, &bytes).expect("native reader");
        let ArtifactBody::Canvas(document) = &decoded.artifact.body else { panic!("Canvas body") };
        assert_eq!(document.layout_guides.get("page"), Some(&guides));
        assert!(feature_keys_of(&decoded.artifact).contains(&FEATURE_LAYOUT_GUIDES.to_owned()));
    }
}
