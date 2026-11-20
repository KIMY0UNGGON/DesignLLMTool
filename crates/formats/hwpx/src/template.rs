//! Blank HWPX package for new documents.
//!
//! The initial archive is written here with a stored `mimetype` entry. The shared package writer
//! deflates new entries, so this is the one place the engine writes a ZIP directly. Edits and saves
//! go through the shared package and keep the stored `mimetype` byte for byte.
//!
//! The XML skeleton carries the element set and the order that Hancom Office writes in its own
//! documents: the header ends with `hh:compatibleDocument` (with its layout child), `hh:docOption`, and
//! `hh:trackchageConfig`; the page section properties of the first run are complete; and the container
//! lists `META-INF/container.rdf`. `tests/l_created_package_structure.rs` checks these items.
use std::io::{Cursor, Write};

use designllm_contracts::{DltResult, ErrorCode};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::diag;
use crate::package::{CONTAINER, CONTAINER_RDF, CONTENT_HPF, HEADER, MIMETYPE, PREVIEW_TEXT, SETTINGS};

/// Font used by new documents. The renderer records the substitution when it is not installed.
pub const DEFAULT_FONT: &str = "Pretendard";

/// Default A4 page size in HWPUNIT, matching the page of Hancom's default blank document.
pub const A4_WIDTH_HU: i64 = 59528;
pub const A4_HEIGHT_HU: i64 = 84186;
/// Default margins in HWPUNIT: left, right, top, bottom.
pub const DEFAULT_MARGINS_HU: (i64, i64, i64, i64) = (8504, 8504, 5668, 4252);

const VERSION_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><hv:HCFVersion xmlns:hv="http://www.hancom.co.kr/hwpml/2011/version" tagetApplication="WORDPROCESSOR" major="5" minor="1" micro="1" buildNumber="0" os="1" xmlVersion="1.5" application="DesignLLMTool" appVersion="0.1.0"/>"#;

const CONTAINER_XML: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><ocf:container xmlns:ocf="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:hpf="http://www.hancom.co.kr/schema/2011/hpf"><ocf:rootfiles><ocf:rootfile full-path="Contents/content.hpf" media-type="application/hwpml-package+xml"/><ocf:rootfile full-path="Preview/PrvText.txt" media-type="text/plain"/><ocf:rootfile full-path="META-INF/container.rdf" media-type="application/rdf+xml"/></ocf:rootfiles></ocf:container>"##;

/// Describes the package: the document, its header part, and its section parts.
const CONTAINER_RDF_XML: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:hpkg="http://www.hancom.co.kr/hwpml/2016/meta/pkg#"><rdf:Description rdf:about=""><hpkg:hasPart rdf:resource="Contents/header.xml"/></rdf:Description><rdf:Description rdf:about="Contents/header.xml"><rdf:type rdf:resource="http://www.hancom.co.kr/hwpml/2016/meta/pkg#HeaderFile"/></rdf:Description><rdf:Description rdf:about=""><hpkg:hasPart rdf:resource="Contents/section0.xml"/></rdf:Description><rdf:Description rdf:about="Contents/section0.xml"><rdf:type rdf:resource="http://www.hancom.co.kr/hwpml/2016/meta/pkg#SectionFile"/></rdf:Description><rdf:Description rdf:about=""><rdf:type rdf:resource="http://www.hancom.co.kr/hwpml/2016/meta/pkg#Document"/></rdf:Description></rdf:RDF>"##;

const MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><odf:manifest xmlns:odf="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"/>"#;

const SETTINGS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><ha:HWPApplicationSetting xmlns:ha="http://www.hancom.co.kr/hwpml/2011/app" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0"><ha:CaretPosition listIDRef="0" paraIDRef="0" pos="0"/></ha:HWPApplicationSetting>"#;

const CONTENT_HPF_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><opf:package xmlns:ha="http://www.hancom.co.kr/hwpml/2011/app" xmlns:hp="http://www.hancom.co.kr/hwpml/2011/paragraph" xmlns:hs="http://www.hancom.co.kr/hwpml/2011/section" xmlns:hc="http://www.hancom.co.kr/hwpml/2011/core" xmlns:hh="http://www.hancom.co.kr/hwpml/2011/head" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf/" version="" unique-identifier="" id=""><opf:metadata><opf:title/><opf:language>ko</opf:language></opf:metadata><opf:manifest><opf:item id="header" href="Contents/header.xml" media-type="application/xml"/><opf:item id="section0" href="Contents/section0.xml" media-type="application/xml"/><opf:item id="settings" href="settings.xml" media-type="application/xml"/></opf:manifest><opf:spine><opf:itemref idref="header" linear="yes"/><opf:itemref idref="section0" linear="yes"/></opf:spine></opf:package>"#;

/// Footnote and endnote settings of the section properties, as Hancom Office writes them.
const FOOT_NOTE: &str = r##"<hp:autoNumFormat type="DIGIT" userChar="" prefixChar="" suffixChar=")" supscript="0"/><hp:noteLine length="-1" type="SOLID" width="0.12 mm" color="#000000"/><hp:noteSpacing betweenNotes="283" belowLine="567" aboveLine="850"/><hp:numbering type="CONTINUOUS" newNum="1"/><hp:placement place="EACH_COLUMN" beneathText="0"/>"##;
const END_NOTE: &str = r##"<hp:autoNumFormat type="DIGIT" userChar="" prefixChar="" suffixChar=")" supscript="0"/><hp:noteLine length="14692344" type="SOLID" width="0.12 mm" color="#000000"/><hp:noteSpacing betweenNotes="0" belowLine="567" aboveLine="850"/><hp:numbering type="CONTINUOUS" newNum="1"/><hp:placement place="END_OF_DOCUMENT" beneathText="0"/>"##;

fn font_face(lang: &str) -> String {
    let type_info = crate::header::DEFAULT_TYPE_INFO;
    format!(r##"<hh:fontface lang="{lang}" fontCnt="1"><hh:font id="0" face="{DEFAULT_FONT}" type="TTF" isEmbedded="0">{type_info}</hh:font></hh:fontface>"##)
}

/// Border fill 1 has no fill. Border fill 2 has no visible fill either, and it carries a fill brush record as Hancom
/// Office writes it. The character and paragraph properties refer to border fill 2, the page borders to border fill 1.
fn border_fill(id: u32, fill_brush: bool) -> String {
    let side = |tag: &str| format!(r##"<hh:{tag} type="NONE" width="0.1 mm" color="#000000"/>"##);
    let brush = if fill_brush {
        r##"<hc:fillBrush><hc:winBrush faceColor="none" hatchColor="#999999" alpha="0"/></hc:fillBrush>"##
    } else {
        ""
    };
    format!(
        r##"<hh:borderFill id="{id}" threeD="0" shadow="0" centerLine="NONE" breakCellSeparateLine="0"><hh:slash type="NONE" Crooked="0" isCounter="0"/><hh:backSlash type="NONE" Crooked="0" isCounter="0"/>{}{}{}{}<hh:diagonal type="SOLID" width="0.1 mm" color="#000000"/>{brush}</hh:borderFill>"##,
        side("leftBorder"),
        side("rightBorder"),
        side("topBorder"),
        side("bottomBorder"),
    )
}

fn char_pr() -> String {
    r##"<hh:charPr id="0" height="1000" textColor="#000000" shadeColor="none" useFontSpace="0" useKerning="0" symMark="NONE" borderFillIDRef="2"><hh:fontRef hangul="0" latin="0" hanja="0" japanese="0" other="0" symbol="0" user="0"/><hh:ratio hangul="100" latin="100" hanja="100" japanese="100" other="100" symbol="100" user="100"/><hh:spacing hangul="0" latin="0" hanja="0" japanese="0" other="0" symbol="0" user="0"/><hh:relSz hangul="100" latin="100" hanja="100" japanese="100" other="100" symbol="100" user="100"/><hh:offset hangul="0" latin="0" hanja="0" japanese="0" other="0" symbol="0" user="0"/><hh:underline type="NONE" shape="SOLID" color="#000000"/><hh:strikeout shape="NONE" color="#000000"/><hh:outline type="NONE"/><hh:shadow type="NONE" color="#C0C0C0" offsetX="10" offsetY="10"/></hh:charPr>"##.to_string()
}

/// A paragraph property. Its margin and line spacing are written in the `hp:case` and `hp:default` branches of the
/// markup-compatibility switch, as Hancom Office writes them.
fn para_pr(id: u32, align: &str) -> String {
    let margin = r##"<hh:margin><hc:intent value="0" unit="HWPUNIT"/><hc:left value="0" unit="HWPUNIT"/><hc:right value="0" unit="HWPUNIT"/><hc:prev value="0" unit="HWPUNIT"/><hc:next value="0" unit="HWPUNIT"/></hh:margin><hh:lineSpacing type="PERCENT" value="160" unit="HWPUNIT"/>"##;
    format!(
        r##"<hh:paraPr id="{id}" tabPrIDRef="0" condense="0" fontLineHeight="0" snapToGrid="1" suppressLineNumbers="0" checked="0" textDir="LTR"><hh:align horizontal="{align}" vertical="BASELINE"/><hh:heading type="NONE" idRef="0" level="0"/><hh:breakSetting breakLatinWord="KEEP_WORD" breakNonLatinWord="KEEP_WORD" widowOrphan="0" keepWithNext="0" keepLines="0" pageBreakBefore="0" lineWrap="BREAK"/><hh:autoSpacing eAsianEng="0" eAsianNum="0"/><hp:switch><hp:case hp:required-namespace="http://www.hancom.co.kr/hwpml/2016/HwpUnitChar">{margin}</hp:case><hp:default>{margin}</hp:default></hp:switch><hh:border borderFillIDRef="2" offsetLeft="0" offsetRight="0" offsetTop="0" offsetBottom="0" connect="0" ignoreMargin="0"/></hh:paraPr>"##
    )
}

/// Outline numbering with ten levels, in the formats Hancom Office uses for outline numbering. Levels 9 and 10 carry no
/// text.
fn numbering() -> String {
    const LEVELS: [(&str, &str, u8); 10] = [
        ("DIGIT", "^1.", 0),
        ("HANGUL_SYLLABLE", "^2.", 0),
        ("DIGIT", "^3)", 0),
        ("HANGUL_SYLLABLE", "^4)", 0),
        ("DIGIT", "(^5)", 0),
        ("HANGUL_SYLLABLE", "(^6)", 0),
        ("CIRCLED_DIGIT", "^7", 1),
        ("CIRCLED_HANGUL_SYLLABLE", "^8", 1),
        ("HANGUL_JAMO", "", 0),
        ("ROMAN_SMALL", "", 1),
    ];
    let heads: String = LEVELS
        .iter()
        .enumerate()
        .map(|(index, (format, text, checkable))| {
            let level = index + 1;
            format!(
                r##"<hh:paraHead start="1" level="{level}" align="LEFT" useInstWidth="1" autoIndent="1" widthAdjust="0" textOffsetType="PERCENT" textOffset="50" numFormat="{format}" charPrIDRef="4294967295" checkable="{checkable}">{text}</hh:paraHead>"##
            )
        })
        .collect();
    format!(r##"<hh:numberings itemCnt="1"><hh:numbering id="1" start="0">{heads}</hh:numbering></hh:numberings>"##)
}

fn header_xml() -> String {
    let fontfaces: String = crate::header::LANGS.iter().map(|lang| font_face(lang)).collect();
    let border_fills = format!(r##"<hh:borderFills itemCnt="2">{}{}</hh:borderFills>"##, border_fill(1, false), border_fill(2, true));
    let chars = format!(r##"<hh:charProperties itemCnt="1">{}</hh:charProperties>"##, char_pr());
    let tabs = r##"<hh:tabProperties itemCnt="1"><hh:tabPr id="0" autoTabLeft="0" autoTabRight="0"/></hh:tabProperties>"##;
    let paras = format!(r##"<hh:paraProperties itemCnt="2">{}{}</hh:paraProperties>"##, para_pr(0, "JUSTIFY"), para_pr(1, "LEFT"));
    let styles = r##"<hh:styles itemCnt="2"><hh:style id="0" type="PARA" name="바탕글" engName="Normal" paraPrIDRef="0" charPrIDRef="0" nextStyleIDRef="0" langID="1042" lockForm="0"/><hh:style id="1" type="PARA" name="본문" engName="Body" paraPrIDRef="1" charPrIDRef="0" nextStyleIDRef="1" langID="1042" lockForm="0"/></hh:styles>"##;
    let numbering = numbering();
    format!(
        r##"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><hh:head xmlns:ha="http://www.hancom.co.kr/hwpml/2011/app" xmlns:hp="http://www.hancom.co.kr/hwpml/2011/paragraph" xmlns:hs="http://www.hancom.co.kr/hwpml/2011/section" xmlns:hc="http://www.hancom.co.kr/hwpml/2011/core" xmlns:hh="http://www.hancom.co.kr/hwpml/2011/head" version="1.5" secCnt="1"><hh:beginNum page="1" footnote="1" endnote="1" pic="1" tbl="1" equation="1"/><hh:refList><hh:fontfaces itemCnt="{faces}">{fontfaces}</hh:fontfaces>{border_fills}{chars}{tabs}{numbering}{paras}{styles}</hh:refList><hh:compatibleDocument targetProgram="HWP201X"><hh:layoutCompatibility/></hh:compatibleDocument><hh:docOption><hh:linkinfo path="" pageInherit="0" footnoteInherit="0"/></hh:docOption><hh:trackchageConfig flags="56"/></hh:head>"##,
        faces = crate::header::LANGS.len(),
    )
}

/// A page border fill of the section. Hancom Office writes one each for both, even, and odd pages.
fn page_border_fill(kind: &str) -> String {
    format!(
        r##"<hp:pageBorderFill type="{kind}" borderFillIDRef="1" textBorder="PAPER" headerInside="0" footerInside="0" fillArea="PAPER"><hp:offset left="1417" right="1417" top="1417" bottom="1417"/></hp:pageBorderFill>"##
    )
}

fn section_xml(width_hu: i64, height_hu: i64, margins: (i64, i64, i64, i64)) -> String {
    let (left, right, top, bottom) = margins;
    let page_borders: String = ["BOTH", "EVEN", "ODD"].iter().map(|kind| page_border_fill(kind)).collect();
    format!(
        r##"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?><hs:sec xmlns:ha="http://www.hancom.co.kr/hwpml/2011/app" xmlns:hp="http://www.hancom.co.kr/hwpml/2011/paragraph" xmlns:hs="http://www.hancom.co.kr/hwpml/2011/section" xmlns:hc="http://www.hancom.co.kr/hwpml/2011/core" xmlns:hh="http://www.hancom.co.kr/hwpml/2011/head"><hp:p id="0" paraPrIDRef="0" styleIDRef="0" pageBreak="0" columnBreak="0" merged="0"><hp:run charPrIDRef="0"><hp:secPr id="" textDirection="HORIZONTAL" spaceColumns="1134" tabStop="8000" tabStopVal="4000" tabStopUnit="HWPUNIT" outlineShapeIDRef="1" memoShapeIDRef="0" textVerticalWidthHead="0" masterPageCnt="0"><hp:grid lineGrid="0" charGrid="0" wonggojiFormat="0"/><hp:startNum pageStartsOn="BOTH" page="0" pic="0" tbl="0" equation="0"/><hp:visibility hideFirstHeader="0" hideFirstFooter="0" hideFirstMasterPage="0" border="SHOW_ALL" fill="SHOW_ALL" hideFirstPageNum="0" hideFirstEmptyLine="0" showLineNumber="0"/><hp:lineNumberShape restartType="0" countBy="0" distance="0" startNumber="0"/><hp:pagePr landscape="WIDELY" width="{width_hu}" height="{height_hu}" gutterType="LEFT_ONLY"><hp:margin header="4252" footer="4252" gutter="0" left="{left}" right="{right}" top="{top}" bottom="{bottom}"/></hp:pagePr><hp:footNotePr>{FOOT_NOTE}</hp:footNotePr><hp:endNotePr>{END_NOTE}</hp:endNotePr>{page_borders}</hp:secPr><hp:ctrl><hp:colPr id="" type="NEWSPAPER" layout="LEFT" colCount="1" sameSz="1" sameGap="0"/></hp:ctrl></hp:run></hp:p></hs:sec>"##
    )
}

/// The initial archive of a new document with the given page size and margins (HWPUNIT).
pub fn blank_package(width_hu: i64, height_hu: i64, margins: (i64, i64, i64, i64)) -> DltResult<Vec<u8>> {
    let parts: Vec<(&str, Vec<u8>)> = vec![
        ("version.xml", VERSION_XML.as_bytes().to_vec()),
        (CONTAINER, CONTAINER_XML.as_bytes().to_vec()),
        (CONTAINER_RDF, CONTAINER_RDF_XML.as_bytes().to_vec()),
        ("META-INF/manifest.xml", MANIFEST_XML.as_bytes().to_vec()),
        (CONTENT_HPF, CONTENT_HPF_XML.as_bytes().to_vec()),
        (HEADER, header_xml().into_bytes()),
        ("Contents/section0.xml", section_xml(width_hu, height_hu, margins).into_bytes()),
        (SETTINGS, SETTINGS_XML.as_bytes().to_vec()),
        (PREVIEW_TEXT, Vec::new()),
    ];
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let zip_err = |what: &str, e: &dyn std::fmt::Display| diag::error(ErrorCode::Internal, diag::PACKAGE_INVALID, format!("{what}: {e}"));
    writer
        .start_file("mimetype", SimpleFileOptions::default().compression_method(CompressionMethod::Stored))
        .map_err(|e| zip_err("mimetype", &e))?;
    writer.write_all(MIMETYPE.as_bytes()).map_err(|e| zip_err("mimetype", &e))?;
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in parts {
        writer.start_file(name, deflated).map_err(|e| zip_err(name, &e))?;
        writer.write_all(&bytes).map_err(|e| zip_err(name, &e))?;
    }
    let cursor = writer.finish().map_err(|e| zip_err("archive", &e))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_package_starts_with_a_stored_mimetype() {
        let bytes = blank_package(A4_WIDTH_HU, A4_HEIGHT_HU, DEFAULT_MARGINS_HU).expect("template");
        crate::package::check_mimetype(&bytes).expect("stored mimetype");
    }
}
