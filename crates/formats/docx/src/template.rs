//! Package for a new, empty document. The bytes are a real DOCX package built with the shared package
//! helper. A new document is opened from these bytes like an imported file, so every later save is a
//! patch of an original package.

use designllm_contracts::{DltError, DltResult, PageSetup, Size, Style, EMU_PER_MM};
use designllm_package::opc::{CT_CORE_PROPS, CT_EXT_PROPS, REL_CORE_PROPS, REL_EXT_PROPS};
use designllm_package::xml::escape_text;
use designllm_package::Package;

use crate::tree::{NS_A, NS_PIC, NS_R, NS_W, NS_WP};
use crate::units;
use crate::write::{
    CONTENT_TYPES_PART, CT_DOCUMENT_MAIN, CT_STYLES, DOCUMENT_RELS_PART, MAIN_DOCUMENT_PART, REL_OFFICE_DOCUMENT, REL_STYLES,
    STYLES_PART,
};

pub const DOCX_MIME: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

const CT_SETTINGS: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml";
const REL_SETTINGS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings";

/// Builds a blank document package with the given page setup (EMU values, applied as twips) and title.
pub fn blank_package(size: Option<Size>, title: &str) -> DltResult<Vec<u8>> {
    let page = match size {
        Some(size) => {
            if !size.valid() {
                return Err(DltError::invalid("page size must be positive and within 10 m"));
            }
            size
        }
        None => Size::a4(),
    };
    let margin = 20 * EMU_PER_MM;
    let setup = PageSetup { size: page, margin_left: margin, margin_top: margin, margin_right: margin, margin_bottom: margin };
    let (w, h) = (units::to_twips(page.width)?.0, units::to_twips(page.height)?.0);
    let landscape = if page.width > page.height { " w:orient=\"landscape\"" } else { "" };
    let margins = [
        units::to_twips(setup.margin_top)?.0,
        units::to_twips(setup.margin_right)?.0,
        units::to_twips(setup.margin_bottom)?.0,
        units::to_twips(setup.margin_left)?.0,
    ];
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:document xmlns:w=\"{NS_W}\" xmlns:r=\"{NS_R}\" xmlns:wp=\"{NS_WP}\" xmlns:a=\"{NS_A}\" xmlns:pic=\"{NS_PIC}\"><w:body><w:p/><w:sectPr><w:pgSz w:w=\"{w}\" w:h=\"{h}\"{landscape}/><w:pgMar w:top=\"{}\" w:right=\"{}\" w:bottom=\"{}\" w:left=\"{}\" w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>",
        margins[0], margins[1], margins[2], margins[3]
    );
    let font = Style::default().font_family;
    let font = escape_text(&font)?;
    let color = units::color_file(&Style::default().color)?;
    let half_points = units::to_half_points(Style::default().font_size)?.0;
    let styles = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:styles xmlns:w=\"{NS_W}\"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"{font}\" w:hAnsi=\"{font}\" w:eastAsia=\"{font}\" w:cs=\"{font}\"/><w:color w:val=\"{color}\"/><w:sz w:val=\"{half_points}\"/><w:szCs w:val=\"{half_points}\"/><w:lang w:val=\"en-US\" w:eastAsia=\"ko-KR\"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after=\"120\" w:line=\"240\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style><w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before=\"240\" w:after=\"120\"/><w:outlineLvl w:val=\"0\"/></w:pPr><w:rPr><w:b/><w:sz w:val=\"32\"/><w:szCs w:val=\"32\"/></w:rPr></w:style><w:style w:type=\"character\" w:default=\"1\" w:styleId=\"DefaultParagraphFont\"><w:name w:val=\"Default Paragraph Font\"/><w:uiPriority w:val=\"1\"/><w:semiHidden/><w:unhideWhenUsed/></w:style></w:styles>"
    );
    let settings = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:settings xmlns:w=\"{NS_W}\"><w:defaultTabStop w:val=\"720\"/></w:settings>");
    let title = escape_text(title)?;
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{title}</dc:title></cp:coreProperties>"
    );
    let app = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>DesignLLMTool</Application></Properties>";
    let content_types = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/{MAIN_DOCUMENT_PART}\" ContentType=\"{CT_DOCUMENT_MAIN}\"/><Override PartName=\"/{STYLES_PART}\" ContentType=\"{CT_STYLES}\"/><Override PartName=\"/word/settings.xml\" ContentType=\"{CT_SETTINGS}\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"{CT_CORE_PROPS}\"/><Override PartName=\"/docProps/app.xml\" ContentType=\"{CT_EXT_PROPS}\"/></Types>"
    );
    let root_rels = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL_OFFICE_DOCUMENT}\" Target=\"{MAIN_DOCUMENT_PART}\"/><Relationship Id=\"rId2\" Type=\"{REL_CORE_PROPS}\" Target=\"docProps/core.xml\"/><Relationship Id=\"rId3\" Type=\"{REL_EXT_PROPS}\" Target=\"docProps/app.xml\"/></Relationships>"
    );
    let document_rels = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL_STYLES}\" Target=\"styles.xml\"/><Relationship Id=\"rId2\" Type=\"{REL_SETTINGS}\" Target=\"settings.xml\"/></Relationships>"
    );

    let mut package = Package::new();
    package.set_part(CONTENT_TYPES_PART, content_types.into_bytes())?;
    package.set_part("_rels/.rels", root_rels.into_bytes())?;
    package.set_part(MAIN_DOCUMENT_PART, document.into_bytes())?;
    package.set_part(DOCUMENT_RELS_PART, document_rels.into_bytes())?;
    package.set_part(STYLES_PART, styles.into_bytes())?;
    package.set_part("word/settings.xml", settings.into_bytes())?;
    package.set_part("docProps/core.xml", core.into_bytes())?;
    package.set_part("docProps/app.xml", app.as_bytes().to_vec())?;
    package.encode()
}

#[cfg(test)]
mod tests {
    use designllm_contracts::{sha256, Size, EMU_PER_MM};

    /// Golden digests of the new-document package, recorded before the OOXML constants moved to the package
    /// crate (task 49k). A changed constant value or template byte changes them.
    #[test]
    fn the_blank_package_bytes_match_the_golden_digests() {
        let portrait = super::blank_package(None, "제목 & <테스트>").unwrap();
        let landscape = super::blank_package(Some(Size { width: 297 * EMU_PER_MM, height: 210 * EMU_PER_MM }), "t").unwrap();
        assert_eq!(sha256(&portrait), "ed48981f13d744108f2f6fbb91f8bc9a8d5fc6c1658ea35775862ea8f103684e");
        assert_eq!(sha256(&landscape), "d9d17cd69be6c9dd2cdca984d12a1cec3f4ef84a2978b4f4a5040973a3a2c879");
    }

    /// The names that template.rs and verify.rs each defined before task 49k keep their strings.
    #[test]
    fn the_shared_wordprocessingml_names_keep_their_values() {
        assert_eq!(super::CT_DOCUMENT_MAIN, "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml");
        assert_eq!(super::CT_STYLES, "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml");
        assert_eq!(super::REL_STYLES, "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles");
    }
}
