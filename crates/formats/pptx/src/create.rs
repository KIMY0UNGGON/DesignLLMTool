//! Creates PresentationML packages: a blank deck for new slide artifacts, and a deck built from a render scene
//! for figure export (`encode_scene`).
//!
//! Every created package uses the same skeleton: one slide master, one blank layout, one theme, and the
//! document properties. Text, shapes, lines, and pictures are native PresentationML objects.

use std::collections::BTreeSet;

use designllm_contracts::{
    sha256, AssetStore, Diagnostic, DltError, DltResult, EncodedOutput, Editability, ErrorCode, PixelRect, Primitive, RenderScene, Size,
    Style,
};
use designllm_package::opc::{self, ContentTypes, Relationship};
use designllm_package::xml as pxml;
use designllm_package::Package;
use designllm_render::face_matches_family;

use crate::ns::{self, NS_A, NS_CP, NS_DC, NS_DCTERMS, NS_EXT_PROPS, NS_P, NS_R, NS_VT, NS_XSI};
use crate::v12::{self, Kind};
use crate::writer::xfrm_xml;

/// Shape presets that scene shapes may name. Anything else is refused rather than approximated.
pub const SCENE_PRESETS: &[&str] = &[
    "rect", "roundRect", "ellipse", "triangle", "rtTriangle", "parallelogram", "trapezoid", "diamond", "pentagon",
    "hexagon", "octagon", "star5", "line", "straightConnector1", "rightArrow", "leftArrow", "upArrow", "downArrow",
    "chevron", "homePlate", "flowChartProcess", "flowChartDecision", "flowChartTerminator", "flowChartDocument",
    "flowChartConnector", "can", "cube", "donut", "heart", "cloud",
];

const THEME_XML: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="DesignLLMTool"><a:themeElements><a:clrScheme name="DesignLLMTool"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="20252D"/></a:dk2><a:lt2><a:srgbClr val="F5F7FA"/></a:lt2><a:accent1><a:srgbClr val="315A7D"/></a:accent1><a:accent2><a:srgbClr val="5B9BD5"/></a:accent2><a:accent3><a:srgbClr val="ED7D31"/></a:accent3><a:accent4><a:srgbClr val="A5A5A5"/></a:accent4><a:accent5><a:srgbClr val="FFC000"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="DesignLLMTool"><a:majorFont><a:latin typeface="Pretendard"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Pretendard"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="DesignLLMTool"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"##;

const MASTER_CLR_MAP: &str = "bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"";

fn group_header() -> String {
    "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>".to_string()
}

fn master_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sldMaster xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"><p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree>{}</p:spTree></p:cSld><p:clrMap {MASTER_CLR_MAP}/><p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr algn=\"l\"><a:defRPr sz=\"4400\"/></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:defRPr sz=\"2800\"/></a:lvl1pPr></p:bodyStyle><p:otherStyle><a:defPPr><a:defRPr lang=\"en-US\"/></a:defPPr><a:lvl1pPr marL=\"0\" algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:defRPr sz=\"1800\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mn-lt\"/><a:ea typeface=\"+mn-ea\"/><a:cs typeface=\"+mn-cs\"/></a:defRPr></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>",
        group_header()
    )
}

fn layout_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sldLayout xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\" type=\"blank\" preserve=\"1\"><p:cSld name=\"Blank\"><p:spTree>{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>",
        group_header()
    )
}

/// One slide of a created deck: the shapes inside `p:spTree` and the pictures it references.
pub(crate) struct SlideSpec {
    pub shapes: String,
    pub pictures: Vec<PictureSpec>,
}

pub(crate) struct PictureSpec {
    pub rid: String,
    pub part: String,
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Builds a complete package with the given slides. Slide `k` is `ppt/slides/slide{k+1}.xml`.
pub(crate) fn build_deck(title: &str, size: Size, slides: &[SlideSpec]) -> DltResult<Vec<u8>> {
    if slides.is_empty() {
        return Err(DltError::invalid("a presentation needs at least one slide"));
    }
    let mut pkg = Package::new();
    let mut ct = ContentTypes::default();
    ct.set_default("rels", ns::CT_RELATIONSHIPS);
    ct.set_default("xml", "application/xml");

    // Package root and document properties.
    pkg.set_part(
        "_rels/.rels",
        opc::rels_to_xml(&[
            Relationship { id: "rId1".into(), rel_type: ns::REL_OFFICE_DOCUMENT.into(), target: "ppt/presentation.xml".into(), external: false },
            Relationship { id: "rId2".into(), rel_type: ns::REL_CORE_PROPS.into(), target: "docProps/core.xml".into(), external: false },
            Relationship { id: "rId3".into(), rel_type: ns::REL_EXT_PROPS.into(), target: "docProps/app.xml".into(), external: false },
        ]),
    )?;
    let title_escaped = pxml::escape_text(title)?;
    pkg.set_part(
        "docProps/core.xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"{NS_CP}\" xmlns:dc=\"{NS_DC}\" xmlns:dcterms=\"{NS_DCTERMS}\" xmlns:xsi=\"{NS_XSI}\"><dc:title>{title_escaped}</dc:title><dcterms:created xsi:type=\"dcterms:W3CDTF\">2026-11-12T00:00:00Z</dcterms:created><dcterms:modified xsi:type=\"dcterms:W3CDTF\">2026-11-12T00:00:00Z</dcterms:modified></cp:coreProperties>"
        )
        .into_bytes(),
    )?;
    ct.set_override("docProps/core.xml", ns::CT_CORE_PROPS);
    pkg.set_part(
        "docProps/app.xml",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Properties xmlns=\"{NS_EXT_PROPS}\" xmlns:vt=\"{NS_VT}\"><Application>DesignLLMTool</Application><Slides>{}</Slides></Properties>",
            slides.len()
        )
        .into_bytes(),
    )?;
    ct.set_override("docProps/app.xml", ns::CT_EXT_PROPS);

    // Presentation, its relationships, and the slide list.
    let mut pres_rels = vec![Relationship { id: "rId1".into(), rel_type: ns::REL_SLIDE_MASTER.into(), target: "slideMasters/slideMaster1.xml".into(), external: false }];
    let mut slide_list = String::new();
    for (index, _) in slides.iter().enumerate() {
        let rid = format!("rId{}", index + 2);
        pres_rels.push(Relationship { id: rid.clone(), rel_type: ns::REL_SLIDE.into(), target: format!("slides/slide{}.xml", index + 1), external: false });
        slide_list.push_str(&format!("<p:sldId id=\"{}\" r:id=\"{rid}\"/>", 256 + index));
    }
    let after = slides.len() + 2;
    pres_rels.push(Relationship { id: format!("rId{after}"), rel_type: ns::REL_PRES_PROPS.into(), target: "presProps.xml".into(), external: false });
    pres_rels.push(Relationship { id: format!("rId{}", after + 1), rel_type: ns::REL_VIEW_PROPS.into(), target: "viewProps.xml".into(), external: false });
    pres_rels.push(Relationship { id: format!("rId{}", after + 2), rel_type: ns::REL_TABLE_STYLES.into(), target: "tableStyles.xml".into(), external: false });
    pres_rels.push(Relationship { id: format!("rId{}", after + 3), rel_type: ns::REL_THEME.into(), target: "theme/theme1.xml".into(), external: false });
    pkg.set_part(ns::PRESENTATION_RELS_PART, opc::rels_to_xml(&pres_rels))?;
    pkg.set_part(
        ns::PRESENTATION_PART,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:presentation xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\" saveSubsetFonts=\"1\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>{slide_list}</p:sldIdLst><p:sldSz cx=\"{}\" cy=\"{}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/><p:defaultTextStyle><a:defPPr><a:defRPr lang=\"en-US\"/></a:defPPr><a:lvl1pPr marL=\"0\" algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:defRPr sz=\"1800\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mn-lt\"/><a:ea typeface=\"+mn-ea\"/><a:cs typeface=\"+mn-cs\"/></a:defRPr></a:lvl1pPr></p:defaultTextStyle></p:presentation>",
            size.width, size.height
        )
        .into_bytes(),
    )?;
    ct.set_override(ns::PRESENTATION_PART, ns::CT_PRESENTATION);
    pkg.set_part("ppt/presProps.xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:presentationPr xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"/>").into_bytes())?;
    ct.set_override("ppt/presProps.xml", ns::CT_PRES_PROPS);
    pkg.set_part("ppt/viewProps.xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:viewPr xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"/>").into_bytes())?;
    ct.set_override("ppt/viewProps.xml", ns::CT_VIEW_PROPS);
    pkg.set_part("ppt/tableStyles.xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<a:tblStyleLst xmlns:a=\"{NS_A}\" def=\"{{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}}\"/>").into_bytes())?;
    ct.set_override("ppt/tableStyles.xml", ns::CT_TABLE_STYLES);

    // Theme, master, and layout.
    pkg.set_part("ppt/theme/theme1.xml", THEME_XML.as_bytes().to_vec())?;
    ct.set_override("ppt/theme/theme1.xml", ns::CT_THEME);
    pkg.set_part("ppt/slideMasters/slideMaster1.xml", master_xml().into_bytes())?;
    ct.set_override("ppt/slideMasters/slideMaster1.xml", ns::CT_SLIDE_MASTER);
    pkg.set_part(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        opc::rels_to_xml(&[
            Relationship { id: "rId1".into(), rel_type: ns::REL_SLIDE_LAYOUT.into(), target: "../slideLayouts/slideLayout1.xml".into(), external: false },
            Relationship { id: "rId2".into(), rel_type: ns::REL_THEME.into(), target: "../theme/theme1.xml".into(), external: false },
        ]),
    )?;
    pkg.set_part("ppt/slideLayouts/slideLayout1.xml", layout_xml().into_bytes())?;
    ct.set_override("ppt/slideLayouts/slideLayout1.xml", ns::CT_SLIDE_LAYOUT);
    pkg.set_part(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        opc::rels_to_xml(&[Relationship { id: "rId1".into(), rel_type: ns::REL_SLIDE_MASTER.into(), target: "../slideMasters/slideMaster1.xml".into(), external: false }]),
    )?;

    // Slides and their pictures.
    for (index, slide) in slides.iter().enumerate() {
        let part = format!("ppt/slides/slide{}.xml", index + 1);
        pkg.set_part(
            &part,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sld xmlns:a=\"{NS_A}\" xmlns:r=\"{NS_R}\" xmlns:p=\"{NS_P}\"><p:cSld><p:spTree>{}{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>",
                group_header(),
                slide.shapes
            )
            .into_bytes(),
        )?;
        ct.set_override(&part, ns::CT_SLIDE);
        let mut rels = vec![Relationship { id: "rId1".into(), rel_type: ns::REL_SLIDE_LAYOUT.into(), target: "../slideLayouts/slideLayout1.xml".into(), external: false }];
        for picture in &slide.pictures {
            pkg.set_part(&picture.part, picture.bytes.clone())?;
            if let Some(extension) = ns::extension_for_mime(&picture.mime) {
                if ct.defaults.get(extension).is_none() {
                    ct.set_default(extension, &picture.mime);
                }
            }
            rels.push(Relationship {
                id: picture.rid.clone(),
                rel_type: ns::REL_IMAGE.into(),
                target: format!("../media/{}", picture.part.rsplit('/').next().unwrap_or("")),
                external: false,
            });
        }
        pkg.set_part(&opc::rels_part_for(&part), opc::rels_to_xml(&rels))?;
    }
    pkg.set_part(opc::CONTENT_TYPES_PART, ct.to_xml())?;
    pkg.encode()
}

/// Bytes of a blank `.pptx` with one empty slide of the given size.
pub fn blank_deck(title: &str, size: Size) -> DltResult<Vec<u8>> {
    if !size.valid() {
        return Err(DltError::invalid("slide size must be positive and at most 10 m on each side"));
    }
    build_deck(title, size, &[SlideSpec { shapes: String::new(), pictures: vec![] }])
}

/// Result of [`encode_scene`] before it is wrapped in `EncodedOutput`.
struct SceneBuild {
    slides: Vec<SlideSpec>,
    size: Size,
    diagnostics: Vec<Diagnostic>,
}

/// Writes a render scene as a PPTX. Text, shapes, lines, and pictures become native objects. Chart-like
/// scene content is shapes, not chart data. The output editability is `Objects`.
pub fn encode_scene(scene: &RenderScene, assets: &dyn AssetStore, title: &str) -> DltResult<EncodedOutput> {
    let built = build_scene(scene, assets)?;
    let bytes = build_deck(title, built.size, &built.slides)?;
    // The figure is embedded as soon as it is exported, so the package must pass the OPC check on its own bytes.
    let report = opc::verify(&Package::open(&bytes)?, ns::EXPECTED_TARGET_TYPES);
    if !report.errors.is_empty() {
        let mut err = DltError::new(ErrorCode::Internal, "the generated PPTX failed the package check");
        err.diagnostics = report
            .errors
            .iter()
            .map(|message| Diagnostic { code: "PPTX_OPC_ERROR".into(), message: message.clone(), node_id: None, blocking: true, params: Default::default() })
            .collect();
        return Err(err);
    }
    Ok(EncodedOutput { bytes, mime: ns::PPTX_MIME.to_string(), editability: Editability::Objects, diagnostics: built.diagnostics })
}

fn build_scene(scene: &RenderScene, assets: &dyn AssetStore) -> DltResult<SceneBuild> {
    let first = scene
        .pages
        .first()
        .ok_or_else(|| DltError::invalid("the scene has no pages"))?;
    let size = first.size;
    if !size.valid() {
        return Err(DltError::invalid("the scene page size is not valid"));
    }
    let mut diagnostics: Vec<Diagnostic> = scene.diagnostics.clone();
    // A node whose substitution the scene already reports is not reported again by this export.
    let mut substituted: BTreeSet<String> = scene
        .diagnostics
        .iter()
        .filter(|d| d.code == designllm_contracts::diag_codes::FONT_SUBSTITUTED)
        .filter_map(|d| d.node_id.clone())
        .collect();
    if scene.metadata.keys().any(|k| k.starts_with("plot")) {
        diagnostics.push(Diagnostic {
            code: "PPTX_CHART_AS_SHAPES".into(),
            message: "charts in this scene are written as native shapes and text, not as chart data".into(),
            node_id: None,
            blocking: false,
            params: Default::default(),
        });
    }
    let mut slides = Vec::with_capacity(scene.pages.len());
    let mut picture_counter = 0usize;
    for page in &scene.pages {
        if page.size != size {
            return Err(DltError::unsupported("pages of different sizes cannot share one presentation"));
        }
        let mut shapes = String::new();
        let mut pictures = Vec::new();
        let mut next_id = 2u32;
        for primitive in &page.primitives {
            match primitive {
                // A path has no text, so its style is written as it is: no font size check and no rounding. A path that the
                // canonical custom geometry cannot hold is refused before anything is written.
                Primitive::Path { node_id, bounds, path, style } => {
                    crate::guard::check_path(path, node_id, "in PPTX export")?;
                    shapes.push_str(&path_xml(next_id, node_id, bounds, path, style, &mut diagnostics)?);
                }
                Primitive::Text { node_id, bounds, origin, text, style, shaped } => {
                    let (style, rounded) = quantize_style(style, node_id)?;
                    if let Some(d) = rounded {
                        diagnostics.push(d);
                    }
                    // Text drawn with no face that carries its requested family name, localized names included, was
                    // shaped with a substitute. The name it was drawn with is reported.
                    let requested = style.font_family.as_str();
                    if !requested.trim().is_empty()
                        && !text.trim().is_empty()
                        && !shaped.fonts_used.is_empty()
                        && !shaped.fonts_used.iter().any(|face| face_matches_family(face, requested))
                        && !substituted.contains(node_id)
                    {
                        let drawn: BTreeSet<&str> = shaped
                            .fonts_used
                            .iter()
                            .map(|face| if face.family.is_empty() { face.postscript_name.as_str() } else { face.family.as_str() })
                            .collect();
                        let drawn = drawn.into_iter().collect::<Vec<_>>().join(", ");
                        diagnostics.push(Diagnostic {
                            code: designllm_contracts::diag_codes::FONT_SUBSTITUTED.into(),
                            message: format!("font {requested} is not available; the text is drawn with {drawn}"),
                            node_id: Some(node_id.clone()),
                            blocking: false,
                            params: Default::default(),
                        });
                        substituted.insert(node_id.clone());
                    }
                    let height = (shaped.ascent + shaped.descent).max(1);
                    let top = origin.y - shaped.baseline;
                    let width = bounds.width.max(shaped.size.width).max(1);
                    shapes.push_str(&text_box_xml(next_id, node_id, origin.x, top, width, height, text, &style, &mut diagnostics)?);
                }
                Primitive::Shape { node_id, bounds, shape, style } => {
                    if !SCENE_PRESETS.contains(&shape.as_str()) {
                        return Err(DltError::unsupported(format!("shape preset {shape:?} is not supported in PPTX export")));
                    }
                    shapes.push_str(&preset_xml(next_id, node_id, bounds, shape, style, &mut diagnostics)?);
                }
                Primitive::Line { node_id, from, to, style, arrow } => {
                    shapes.push_str(&line_xml(next_id, node_id, *from, *to, style, *arrow, &mut diagnostics)?);
                }
                Primitive::Image { node_id, bounds, asset, image_crop_pixel_rect, .. } => {
                    let bytes = assets.read(asset)?;
                    if sha256(&bytes) != asset.sha256 || !ns::magic_matches(&asset.mime, &bytes) {
                        return Err(DltError::invalid(format!("picture {node_id} does not match its declared type or hash")));
                    }
                    let extension = ns::extension_for_mime(&asset.mime)
                        .ok_or_else(|| DltError::unsupported(format!("picture format {} is not supported in PPTX export", asset.mime)))?;
                    picture_counter += 1;
                    let rid = format!("rId{}", picture_counter + 1);
                    let part = format!("ppt/media/image{picture_counter}.{extension}");
                    let src_rect = image_crop_pixel_rect
                        .as_ref()
                        .and_then(|crop| src_rect_xml(crop, &asset.mime, &bytes, node_id, &mut diagnostics));
                    shapes.push_str(&picture_xml(next_id, node_id, bounds, &rid, src_rect.as_deref())?);
                    pictures.push(PictureSpec { rid, part, bytes, mime: asset.mime.clone() });
                }
            }
            next_id += 1;
        }
        slides.push(SlideSpec { shapes, pictures });
    }
    // Picture parts are named globally, so each slide's relationships point at the right part.
    Ok(SceneBuild { slides, size, diagnostics })
}

/// Font sizes must sit on the 1/100 pt grid. A size off the grid is rounded to the nearest grid value and reported.
fn quantize_style(style: &Style, node_id: &str) -> DltResult<(Style, Option<Diagnostic>)> {
    if style.font_size <= 0 {
        return Err(DltError::new(ErrorCode::InvalidArgument, format!("{node_id}: font size must be positive")));
    }
    let rounded = ((style.font_size + 63) / 127) * 127;
    if rounded == style.font_size {
        return Ok((style.clone(), None));
    }
    let mut copy = style.clone();
    copy.font_size = rounded;
    let diagnostic = Diagnostic {
        code: designllm_contracts::diag_codes::NATIVE_UNIT_ROUNDING.into(),
        message: format!("{node_id}: font size {} EMU rounded to {rounded} EMU (1/100 pt grid)", style.font_size),
        node_id: Some(node_id.to_string()),
        blocking: false,
        params: Default::default(),
    };
    Ok((copy, Some(diagnostic)))
}

fn text_box_xml(id: u32, node_id: &str, x: i64, y: i64, width: i64, height: i64, text: &str, style: &Style, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let name = pxml::escape_attr(node_id)?;
    let run = pxml::escape_text(text)?;
    v12::unmapped_losses(style, Kind::Text, node_id, losses);
    let paragraph = v12::para_props_xml(style, node_id, losses)?;
    let rpr = v12::run_props_xml(style, node_id, losses)?;
    let anchor = v12::anchor_of(style, node_id, losses);
    Ok(format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{width}\" cy=\"{height}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap=\"none\" lIns=\"0\" tIns=\"0\" rIns=\"0\" bIns=\"0\" anchor=\"{anchor}\" rtlCol=\"0\"/><a:lstStyle/><a:p>{paragraph}<a:r>{rpr}<a:t>{run}</a:t></a:r></a:p></p:txBody></p:sp>"
    ))
}

fn preset_xml(id: u32, node_id: &str, bounds: &designllm_contracts::Rect, shape: &str, style: &Style, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let name = pxml::escape_attr(node_id)?;
    v12::unmapped_losses(style, Kind::Shape, node_id, losses);
    let geometry = v12::geometry_xml(shape, style, bounds, node_id, losses)?;
    let fill = v12::fill_xml(style, node_id, losses)?;
    let line = closed_outline_xml(style, node_id, losses)?;
    let effects = v12::effect_xml(style, node_id, losses)?;
    Ok(format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}{geometry}{fill}{line}{effects}</p:spPr></p:sp>",
        xfrm_xml(bounds, false, false)
    ))
}

/// A path (`Primitive::Path`): custom geometry in the shape's bounds, with the same fill, outline, and effects as a
/// preset shape. A path has no corner radius, so one is reported by the mapping.
fn path_xml(id: u32, node_id: &str, bounds: &designllm_contracts::Rect, path: &designllm_contracts::PathSpec, style: &Style, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let name = pxml::escape_attr(node_id)?;
    v12::unmapped_losses(style, Kind::Path, node_id, losses);
    let geometry = v12::cust_geom_xml(path, node_id, losses)?;
    let fill = v12::fill_xml(style, node_id, losses)?;
    let line = closed_outline_xml(style, node_id, losses)?;
    let effects = v12::effect_xml(style, node_id, losses)?;
    Ok(format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr>{}{geometry}{fill}{line}{effects}</p:spPr></p:sp>",
        xfrm_xml(bounds, false, false)
    ))
}

/// Outline of a closed shape or path. The render draws no outline for a width of zero or less, so none is written for
/// it: PowerPoint would draw a zero width as a hairline, and a negative width is not a valid `a:ln/@w`. A negative width
/// is written as no outline, as before, and reported.
fn closed_outline_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    if style.stroke_width > 0 {
        return v12::ln_xml(style, node_id, losses, "");
    }
    if style.stroke_width < 0 {
        losses.push(v12::loss(node_id, "stroke_width", "a negative width is written as no outline"));
    }
    Ok("<a:ln w=\"0\"><a:noFill/></a:ln>".to_string())
}

fn line_xml(id: u32, node_id: &str, from: designllm_contracts::Point, to: designllm_contracts::Point, style: &Style, arrow: bool, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let name = pxml::escape_attr(node_id)?;
    let bounds = designllm_contracts::Rect {
        x: from.x.min(to.x),
        y: from.y.min(to.y),
        width: (to.x - from.x).abs(),
        height: (to.y - from.y).abs(),
    };
    let flip_h = to.x < from.x;
    let flip_v = to.y < from.y;
    v12::unmapped_losses(style, Kind::Line, node_id, losses);
    let markers = v12::line_end_xml(style);
    let legacy_tail = if arrow && style.stroke_end.is_none() { "<a:tailEnd type=\"triangle\"/>" } else { "" };
    let tail = format!("{markers}{legacy_tail}");
    let ln = v12::ln_xml(style, node_id, losses, &tail)?;
    let effects = v12::effect_xml(style, node_id, losses)?;
    Ok(format!(
        "<p:cxnSp><p:nvCxnSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvCxnSpPr/><p:nvPr/></p:nvCxnSpPr><p:spPr>{}<a:prstGeom prst=\"line\"><a:avLst/></a:prstGeom>{ln}{effects}</p:spPr></p:cxnSp>",
        xfrm_xml(&bounds, flip_h, flip_v)
    ))
}

/// A picture. `src_rect` is the `a:srcRect` of its crop; without one the blip is stretched over the box as it always was.
fn picture_xml(id: u32, node_id: &str, bounds: &designllm_contracts::Rect, rid: &str, src_rect: Option<&str>) -> DltResult<String> {
    let name = pxml::escape_attr(node_id)?;
    let src_rect = src_rect.unwrap_or("");
    Ok(format!(
        "<p:pic><p:nvPicPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed=\"{rid}\"/>{src_rect}<a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>",
        xfrm_xml(bounds, false, false)
    ))
}

/// The `a:srcRect` of a cropped picture, or `None` when the crop is not written. Each edge is a share of the picture's side
/// in 1/1000 %, so 100000 is the whole side, as OOXML defines `srcRect`. An edge that the 1/1000 % grid cannot express
/// exactly is rounded, and that is reported. A crop that the picture cannot hold, and a picture whose size cannot be read,
/// are not written: the whole picture is then drawn in its box, and that is reported.
fn src_rect_xml(crop: &PixelRect, mime: &str, bytes: &[u8], node_id: &str, losses: &mut Vec<Diagnostic>) -> Option<String> {
    let Some((width, height)) = picture_pixel_size(mime, bytes) else {
        losses.push(v12::loss(
            node_id,
            "image_crop_pixel_rect",
            "the picture size cannot be read, so the crop is not written and the whole picture is drawn in its box",
        ));
        return None;
    };
    let (x, y) = (u64::from(crop.x), u64::from(crop.y));
    let (w, h) = (u64::from(crop.width), u64::from(crop.height));
    if w == 0 || h == 0 || x + w > width || y + h > height {
        losses.push(v12::loss(
            node_id,
            "image_crop_pixel_rect",
            "the crop is empty or lies outside the picture, so it is not written and the whole picture is drawn in its box",
        ));
        return None;
    }
    let edges = [edge_milli(x, width), edge_milli(width - x - w, width), edge_milli(y, height), edge_milli(height - y - h, height)];
    if edges.iter().any(|&(_, exact)| !exact) {
        losses.push(v12::loss(node_id, "image_crop_pixel_rect", "an edge is rounded to 1/1000 % of the picture side, which the grid cannot express exactly"));
    }
    let [left, right, top, bottom] = edges.map(|(value, _)| value);
    Some(format!("<a:srcRect l=\"{left}\" t=\"{top}\" r=\"{right}\" b=\"{bottom}\"/>"))
}

/// An edge of `pixels` out of `total` in 1/1000 %, rounded half up, and whether that rounding was exact.
fn edge_milli(pixels: u64, total: u64) -> (u64, bool) {
    let scaled = pixels * 100_000;
    ((scaled + total / 2) / total, scaled % total == 0)
}

/// Pixel width and height from the header of a PNG, JPEG, GIF, or BMP picture. The caller has checked the magic bytes.
fn picture_pixel_size(mime: &str, bytes: &[u8]) -> Option<(u64, u64)> {
    match mime {
        "image/png" => {
            if bytes.get(12..16)? != &b"IHDR"[..] {
                return None;
            }
            Some((be_u32(bytes, 16)?, be_u32(bytes, 20)?))
        }
        "image/gif" => Some((u64::from(le_u16(bytes, 6)?), u64::from(le_u16(bytes, 8)?))),
        "image/bmp" => {
            // The width and height are 32-bit values at offsets 18 and 22 only in BITMAPINFOHEADER (size 40) and the
            // headers that extend it. An OS/2 BITMAPCOREHEADER (size 12) keeps 16-bit values there, so its size is not read.
            let header = u32::from_le_bytes(bytes.get(14..18)?.try_into().ok()?);
            if header < 40 {
                return None;
            }
            let width = i32::from_le_bytes(bytes.get(18..22)?.try_into().ok()?);
            let height = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
            (width > 0 && height != 0).then(|| (u64::from(width.unsigned_abs()), u64::from(height.unsigned_abs())))
        }
        "image/jpeg" => jpeg_pixel_size(bytes),
        _ => None,
    }
}

/// The size from the first frame header (SOF) of a JPEG. Marker segments before it are skipped by their lengths.
fn jpeg_pixel_size(bytes: &[u8]) -> Option<(u64, u64)> {
    if bytes.get(0..2)? != &[0xFF, 0xD8][..] {
        return None;
    }
    let mut at = 2usize;
    loop {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        while *bytes.get(at)? == 0xFF {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        // Markers without a length: the start and the end of the stream, the restart markers, and TEM.
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        let length = usize::from(be_u16(bytes, at)?);
        if length < 2 {
            return None;
        }
        // The frame headers are SOF0 to SOF15, except DHT (C4), JPG (C8), and DAC (CC).
        let frame = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if frame {
            let height = u64::from(be_u16(bytes, at + 3)?);
            let width = u64::from(be_u16(bytes, at + 5)?);
            return (width > 0 && height > 0).then_some((width, height));
        }
        at += length;
    }
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?)))
}
