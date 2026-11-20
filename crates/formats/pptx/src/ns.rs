//! Namespaces, relationship types, content types, and part names used by PresentationML packages.

pub use designllm_package::opc::{
    CT_CORE_PROPS, CT_EXT_PROPS, NS_A, NS_MC, NS_OFFICE_RELATIONSHIPS as NS_R, REL_CORE_PROPS, REL_EXT_PROPS, REL_IMAGE,
    REL_OFFICE_DOCUMENT,
};

pub const NS_P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
pub const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
pub const NS_DCTERMS: &str = "http://purl.org/dc/terms/";
pub const NS_XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
pub const NS_CP: &str = "http://schemas.openxmlformats.org/package/2006/metadata/core-properties";
pub const NS_EXT_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties";
pub const NS_VT: &str = "http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes";

pub const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
pub const REL_SLIDE_MASTER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
pub const REL_SLIDE_LAYOUT: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
pub const REL_THEME: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme";
pub const REL_NOTES_MASTER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster";
pub const REL_NOTES_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
pub const REL_PRES_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps";
pub const REL_VIEW_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/viewProps";
pub const REL_TABLE_STYLES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles";

pub const CT_PRESENTATION: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";
pub const CT_SLIDE: &str = "application/vnd.openxmlformats-officedocument.presentationml.slide+xml";
pub const CT_SLIDE_LAYOUT: &str = "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml";
pub const CT_SLIDE_MASTER: &str = "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml";
pub const CT_NOTES_SLIDE: &str = "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml";
pub const CT_NOTES_MASTER: &str = "application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml";
pub const CT_THEME: &str = "application/vnd.openxmlformats-officedocument.theme+xml";
pub const CT_PRES_PROPS: &str = "application/vnd.openxmlformats-officedocument.presentationml.presProps+xml";
pub const CT_VIEW_PROPS: &str = "application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml";
pub const CT_TABLE_STYLES: &str = "application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml";
pub const CT_RELATIONSHIPS: &str = "application/vnd.openxmlformats-package.relationships+xml";

/// MIME type of a `.pptx` file.
pub const PPTX_MIME: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

pub const PRESENTATION_PART: &str = "ppt/presentation.xml";
pub const PRESENTATION_RELS_PART: &str = "ppt/_rels/presentation.xml.rels";

/// Relationship and content-type expectations checked by the OPC verifier for PresentationML packages.
pub const EXPECTED_TARGET_TYPES: &[(&str, &str)] = &[
    (REL_OFFICE_DOCUMENT, CT_PRESENTATION),
    (REL_SLIDE, CT_SLIDE),
    (REL_SLIDE_MASTER, CT_SLIDE_MASTER),
    (REL_SLIDE_LAYOUT, CT_SLIDE_LAYOUT),
    (REL_THEME, CT_THEME),
    (REL_NOTES_MASTER, CT_NOTES_MASTER),
    (REL_NOTES_SLIDE, CT_NOTES_SLIDE),
    (REL_PRES_PROPS, CT_PRES_PROPS),
    (REL_VIEW_PROPS, CT_VIEW_PROPS),
    (REL_TABLE_STYLES, CT_TABLE_STYLES),
    (REL_CORE_PROPS, CT_CORE_PROPS),
    (REL_EXT_PROPS, CT_EXT_PROPS),
];

/// Relationship types that are image parts for pictures.
/// Maps a picture MIME type to the file extension used for new media parts. `None` when unsupported.
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => return None,
    })
}

/// MIME type for a media part's extension. `None` when the picture format is not modelled.
pub fn mime_for_extension(extension: &str) -> Option<&'static str> {
    Some(match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpeg" | "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// True when the leading bytes match the magic number of the MIME type. Used to refuse mislabelled images.
pub fn magic_matches(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(&designllm_contracts::media_header::PNG_SIGNATURE),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/bmp" => bytes.starts_with(b"BM"),
        _ => false,
    }
}

/// Relationship types whose targets the writer creates, moves, or removes. Every other relationship is
/// kept byte-for-byte in its canonical form and recorded as a preservation region.
pub fn is_managed_rel_type(rel_type: &str) -> bool {
    matches!(
        rel_type,
        REL_SLIDE | REL_SLIDE_MASTER | REL_SLIDE_LAYOUT | REL_THEME | REL_NOTES_MASTER | REL_NOTES_SLIDE | REL_IMAGE
    )
}

#[cfg(test)]
mod tests {
    use designllm_contracts::{sha256, Size};

    /// Golden digest of a new deck, recorded before the shared OOXML constants moved to the package crate
    /// (task 49k). A changed namespace, relationship, or content-type value changes it.
    #[test]
    fn the_blank_deck_bytes_match_the_golden_digest() {
        let deck = crate::create::blank_deck("빈 문서 & <테스트>", Size::slide()).unwrap();
        assert_eq!(sha256(&deck), "b5be767a76129597763c0ce9a92bbef603a20de8ca6194ed5fb9696dc64cf022");
    }
}
