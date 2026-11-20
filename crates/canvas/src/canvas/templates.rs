//! Canvas templates: deterministic builders for nine starting documents. The same name and size always give the same document,
//! with fixed IDs and no network. Facts to fill in are
//! written as bracketed placeholders such as `[이름]`. Every node is an ordinary editable node.
//!
//! The mobile screen, the social post and the poster lay out for the size they are given: their content stacks and fills
//! the page, so every frame size the home screen offers (and any other size that passes lint) works. The landing page has
//! a fixed size.
//!
//! `lint` checks a built or edited document against the rules the templates must meet: text contrast of at least 4.5:1
//! against each colour the background paints (both stops of a gradient), touch targets of at least 44 CSS px for nodes named
//! `button` or `tab`, no clipped text, nothing drawn outside its page or its frame, and no locked nodes.

use designllm_contracts::{
    ArtifactKind, CanvasDocument, Diagnostic, FRAME_PRESETS, Gradient, GradientStop, IconRef, LayoutSpec, Node, NodeExt, NodeKind,
    CONTRAST_MIN, EMU_PER_CSS_PX, PathSpec, Page, Primitive, RenderScene, Rect, Shadow, Size, StrokeCap, StrokeJoin, Style, TemplateInfo, TOUCH_MIN_PX,
};
use std::collections::BTreeMap;

/// Names accepted by `create` with a template.
pub(crate) const TEMPLATES: &[&str] = &["mobile_app", "landing_page", "social_post", "poster_a3", "resume", "login", "dashboard", "invitation", "banner"];

/// EMU per CSS pixel, per millimetre, and per point.
const PX: i64 = EMU_PER_CSS_PX;
const MM: i64 = 36_000;
const PT: i64 = 12_700;

fn px(v: i64) -> i64 {
    v * PX
}

fn text_style(size: i64, color: &str, bold: bool, align: &str) -> Style {
    Style { font_size: size, color: color.into(), bold, align: align.into(), ..Style::default() }
}

/// A fill without a border.
fn plain(fill: &str) -> Style {
    Style { fill: fill.into(), stroke_width: 0, ..Style::default() }
}

/// A fill with a 1 px border.
fn bordered(fill: &str, stroke: &str) -> Style {
    Style { fill: fill.into(), stroke: stroke.into(), stroke_width: px(1), ..Style::default() }
}

/// Sizing for a child of a flow container (its own mode stays absolute).
fn size_spec(width: &str, height: &str) -> Option<LayoutSpec> {
    Some(LayoutSpec { width_mode: width.into(), height_mode: height.into(), ..Default::default() })
}

fn leaf(id: &str, kind: NodeKind, bounds: Rect, style: Style, text: &str, layout: Option<LayoutSpec>) -> Node {
    Node {
        id: id.into(),
        kind,
        bounds,
        text: text.into(),
        style,
        asset: None,
        children: vec![],
        layout,
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: NodeExt::default(),
    }
}

fn container(id: &str, bounds: Rect, style: Style, layout: LayoutSpec, children: Vec<Node>) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Rectangle,
        bounds,
        text: String::new(),
        style,
        asset: None,
        children,
        layout: Some(layout),
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: NodeExt::default(),
    }
}

fn group(id: &str, bounds: Rect, layout: LayoutSpec, children: Vec<Node>) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Group,
        bounds,
        text: String::new(),
        style: Style::default(),
        asset: None,
        children,
        layout: Some(layout),
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: NodeExt::default(),
    }
}

/// A named node (the layer name is what `lint` reads for touch targets).
fn named(mut node: Node, name: &str) -> Node {
    node.ext.name = Some(name.into());
    node
}

fn at(x: i64, y: i64, w: i64, h: i64) -> Rect {
    Rect { x, y, width: w, height: h }
}

fn px_rect(x: i64, y: i64, w: i64, h: i64) -> Rect {
    at(px(x), px(y), px(w), px(h))
}

fn document(page_id: &str, width: i64, height: i64, nodes: Vec<Node>) -> CanvasDocument {
    CanvasDocument {
        pages: vec![Page { nodes, ..Page::new(page_id, Size { width, height }) }],
        styles: BTreeMap::new(),
        ..Default::default()
    }
}

/// The content icons the templates place: the project's own 24 × 24 line icons, each one path.
const ICONS: &[(&str, &str)] = &[
    ("check", include_str!("../../../../design/icons/content/check.svg")),
    ("chart", include_str!("../../../../design/icons/content/chart.svg")),
    ("chevron-right", include_str!("../../../../design/icons/content/chevron-right.svg")),
    ("heart", include_str!("../../../../design/icons/content/heart.svg")),
    ("home", include_str!("../../../../design/icons/content/home.svg")),
    ("info", include_str!("../../../../design/icons/content/info.svg")),
    ("lock", include_str!("../../../../design/icons/content/lock.svg")),
    ("map-pin", include_str!("../../../../design/icons/content/map-pin.svg")),
    ("phone", include_str!("../../../../design/icons/content/phone.svg")),
    ("search", include_str!("../../../../design/icons/content/search.svg")),
    ("user", include_str!("../../../../design/icons/content/user.svg")),
];

/// A content icon as a Path node, `size` EMU square. The path is the icon's own path data on its 24 grid. The stroke is the
/// icon's 1.5 on 24, so it scales with the icon (`size / 16`).
fn icon(id: &str, name: &str, color: &str, size: i64) -> Node {
    let svg = ICONS.iter().find(|(icon_name, _)| *icon_name == name).map(|(_, svg)| *svg).expect("a template icon");
    let start = svg.find(" d=\"").expect("the icon path") + 4;
    let end = start + svg[start..].find('"').expect("the end of the icon path");
    let path = PathSpec::from_svg(&svg[start..end], at(0, 0, 24_000, 24_000)).expect("a valid icon path");
    Node {
        id: id.into(),
        kind: NodeKind::Path,
        bounds: at(0, 0, size, size),
        text: String::new(),
        style: Style {
            fill: "transparent".into(),
            stroke: color.into(),
            stroke_width: (size + 8) / 16,
            stroke_cap: Some(StrokeCap::Round),
            stroke_join: Some(StrokeJoin::Round),
            ..Style::default()
        },
        asset: None,
        children: vec![],
        layout: None,
        interactions: vec![],
        variants: BTreeMap::new(),
        ext: NodeExt {
            path: Some(path),
            icon: Some(IconRef { set: "content".into(), name: name.into(), version: "1".into() }),
            ..NodeExt::default()
        },
    }
}

/// A two-stop linear gradient from `from` to `to`; `from` is the colour of record.
fn graded(from: &str, to: &str, angle_millideg: i32) -> Style {
    Style {
        gradient: Some(Gradient {
            angle_millideg,
            stops: vec![GradientStop { pos_milli: 0, color: from.into() }, GradientStop { pos_milli: 1000, color: to.into() }],
        }),
        ..plain(from)
    }
}

/// A style with a corner radius in EMU.
fn rounded(style: Style, radius: i64) -> Style {
    Style { corner_radius: Some(radius), ..style }
}

/// The shadow of a card: 4 px down, 16 px blur, 10% black.
fn card_shadow() -> Shadow {
    Shadow { x: 0, y: px(4), blur: px(16), spread: 0, color: "#0F172A1A".into() }
}

fn resume_heading(id: &str, text: &str) -> Node {
    leaf(
        id,
        NodeKind::Text,
        at(0, 0, 1, 1),
        Style { font_size: 13 * PT, font_weight: Some(700), color: "#1D4ED8".into(), ..Style::default() },
        text,
        size_spec("fill", "hug"),
    )
}

fn resume_section(id: &str, title: &str, mut body: Vec<Node>) -> Node {
    let mut children = vec![
        resume_heading(&format!("{id}.heading"), title),
        leaf(&format!("{id}.rule"), NodeKind::Rectangle, at(0, 0, 1, px(1)), plain("#CBD5E1"), "", size_spec("fill", "fixed")),
    ];
    children.append(&mut body);
    group(
        id,
        at(0, 0, 1, 1),
        LayoutSpec { mode: "column".into(), gap: 2 * MM, align: "start".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        children,
    )
}

fn resume_entry(id: &str, title: &str, date: &str, detail: &str) -> Node {
    group(
        id,
        at(0, 0, 1, 1),
        LayoutSpec { mode: "column".into(), gap: MM, align: "start".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        vec![
            leaf(
                &format!("{id}.title"),
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { font_size: 11 * PT, font_weight: Some(600), color: "#0F172A".into(), ..Style::default() },
                title,
                size_spec("fill", "hug"),
            ),
            leaf(&format!("{id}.date"), NodeKind::Text, at(0, 0, 1, 1), text_style(9 * PT, "#64748B", false, "left"), date, size_spec("fill", "hug")),
            leaf(
                &format!("{id}.detail"),
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { line_height_milli: Some(1400), ..text_style(10 * PT, "#334155", false, "left") },
                detail,
                size_spec("fill", "hug"),
            ),
        ],
    )
}

/// Fixed A4 resume; all user facts stay as editable placeholders.
fn resume() -> CanvasDocument {
    let header = container(
        "resume.header",
        at(0, 0, 1, 1),
        rounded(plain("#0F172A"), 2 * MM),
        LayoutSpec { mode: "column".into(), gap: 2 * MM, padding_sides: Some([7 * MM; 4]), align: "start".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        vec![
            leaf(
                "resume.name",
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { font_size: 28 * PT, font_weight: Some(700), color: "#FFFFFF".into(), ..Style::default() },
                "[이름]",
                size_spec("fill", "hug"),
            ),
            leaf("resume.role", NodeKind::Text, at(0, 0, 1, 1), text_style(12 * PT, "#DBEAFE", false, "left"), "[직무 / 전문 분야]", size_spec("fill", "hug")),
            leaf(
                "resume.contact",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(9 * PT, "#FFFFFF", false, "left"),
                "[이메일] · [전화번호] · [포트폴리오 주소]",
                size_spec("fill", "hug"),
            ),
        ],
    );
    let sections = vec![
        resume_section(
            "resume.profile",
            "프로필",
            vec![leaf(
                "resume.profile.detail",
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { line_height_milli: Some(1450), ..text_style(10 * PT, "#334155", false, "left") },
                "[경력과 강점을 두세 문장으로 요약하세요]",
                size_spec("fill", "hug"),
            )],
        ),
        resume_section(
            "resume.experience",
            "경력",
            vec![
                resume_entry("resume.experience.1", "[회사명] · [직무]", "[YYYY.MM – YYYY.MM]", "[담당 업무와 성과를 작성하세요]"),
                resume_entry("resume.experience.2", "[회사명] · [직무]", "[YYYY.MM – YYYY.MM]", "[담당 업무와 성과를 작성하세요]"),
            ],
        ),
        resume_section(
            "resume.education",
            "학력",
            vec![resume_entry("resume.education.1", "[학교명] · [전공]", "[졸업 연도]", "[선택 사항: 관련 과정 또는 학업 성과]")],
        ),
        resume_section(
            "resume.skills",
            "기술 및 도구",
            vec![leaf(
                "resume.skills.detail",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(10 * PT, "#334155", false, "left"),
                "[관련 기술과 도구를 구분 기호로 입력하세요]",
                size_spec("fill", "hug"),
            )],
        ),
        resume_section(
            "resume.credentials",
            "자격 및 수상",
            vec![leaf(
                "resume.credentials.detail",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(10 * PT, "#334155", false, "left"),
                "[자격명 또는 수상명] · [연도]",
                size_spec("fill", "hug"),
            )],
        ),
    ];
    let mut nodes = vec![header];
    nodes.extend(sections);
    let size = Size::a4();
    let mut doc = document(
        "resume",
        size.width,
        size.height,
        vec![container(
            "resume.page",
            at(0, 0, size.width, size.height),
            plain("#FFFFFF"),
            LayoutSpec {
                mode: "column".into(),
                gap: 5 * MM,
                padding_sides: Some([12 * MM, 14 * MM, 12 * MM, 14 * MM]),
                align: "start".into(),
                width_mode: "fixed".into(),
                height_mode: "fixed".into(),
                ..Default::default()
            },
            nodes,
        )],
    );
    doc.pages[0].name = Some("이력서".into());
    doc
}

/// Mobile app screen in EMU; content stacks from the top and the tab bar sits at the bottom.
fn login_field(id: &str, placeholder: &str) -> Node {
    container(
        id,
        at(0, 0, 1, px(48)),
        bordered("#FFFFFF", "#CBD5E1"),
        LayoutSpec { mode: "row".into(), align: "center".into(), padding_sides: Some([0, px(12), 0, px(12)]), width_mode: "fill".into(), height_mode: "fixed".into(), ..Default::default() },
        vec![leaf(
            &format!("{id}.placeholder"),
            NodeKind::Text,
            at(0, 0, 1, 1),
            text_style(px(14), "#64748B", false, "left"),
            placeholder,
            size_spec("fill", "hug"),
        )],
    )
}

fn login(width: i64, height: i64) -> CanvasDocument {
    let card_width = (width - 2 * px(24)).min(px(360)).max(px(220));
    let card = container(
        "login.card",
        at(0, 0, card_width, 1),
        rounded(bordered("#FFFFFF", "#E2E8F0"), px(12)),
        LayoutSpec {
            mode: "column".into(),
            gap: px(16),
            padding_sides: Some([px(24); 4]),
            align: "start".into(),
            width_mode: "fixed".into(),
            height_mode: "hug".into(),
            ..Default::default()
        },
        vec![
            leaf("login.brand", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#1D4ED8", true, "left"), "[서비스 이름]", size_spec("fill", "hug")),
            leaf("login.title", NodeKind::Text, at(0, 0, 1, 1), text_style(px(26), "#0F172A", true, "left"), "로그인", size_spec("fill", "hug")),
            leaf("login.subtitle", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#475569", false, "left"), "[서비스 안내 문구]", size_spec("fill", "hug")),
            login_field("login.email", "[이메일 주소]"),
            login_field("login.password", "[비밀번호]"),
            named(
                container(
                    "login.submit",
                    at(0, 0, 1, px(48)),
                    rounded(plain("#2563EB"), px(8)),
                    LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), width_mode: "fill".into(), height_mode: "fixed".into(), ..Default::default() },
                    vec![leaf("login.submit.label", NodeKind::Text, at(0, 0, 1, 1), text_style(px(15), "#FFFFFF", true, "center"), "[계속하기]", size_spec("hug", "hug"))],
                ),
                "button",
            ),
            leaf(
                "login.signup",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(px(13), "#475569", false, "center"),
                "[계정 만들기 안내]",
                size_spec("fill", "hug"),
            ),
        ],
    );
    let page = container(
        "login.page",
        at(0, 0, width, height),
        plain("#F1F5F9"),
        LayoutSpec {
            mode: "column".into(),
            justify: Some(designllm_contracts::Justify::Center),
            align: "center".into(),
            padding_sides: Some([px(24); 4]),
            width_mode: "fixed".into(),
            height_mode: "fixed".into(),
            ..Default::default()
        },
        vec![card],
    );
    document("login", width, height, vec![page])
}

fn dashboard_metric(id: &str, title: &str) -> Node {
    container(
        id,
        at(0, 0, 1, px(132)),
        rounded(bordered("#FFFFFF", "#E2E8F0"), px(8)),
        LayoutSpec {
            mode: "column".into(),
            gap: px(8),
            padding_sides: Some([px(16); 4]),
            align: "start".into(),
            width_mode: "fill".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        vec![
            leaf(&format!("{id}.title"), NodeKind::Text, at(0, 0, 1, 1), text_style(px(13), "#64748B", false, "left"), title, size_spec("fill", "hug")),
            leaf(
                &format!("{id}.value"),
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { font_size: px(24), font_weight: Some(700), color: "#0F172A".into(), ..Style::default() },
                "[수치]",
                size_spec("fill", "hug"),
            ),
            leaf(&format!("{id}.note"), NodeKind::Text, at(0, 0, 1, 1), text_style(px(12), "#475569", false, "left"), "[기간 또는 설명]", size_spec("fill", "hug")),
        ],
    )
}

fn dashboard() -> CanvasDocument {
    let sidebar = container(
        "dashboard.sidebar",
        at(0, 0, px(232), px(900)),
        plain("#0F172A"),
        LayoutSpec {
            mode: "column".into(),
            gap: px(20),
            padding_sides: Some([px(24); 4]),
            align: "start".into(),
            width_mode: "fixed".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        vec![
            leaf("dashboard.brand", NodeKind::Text, at(0, 0, 1, 1), text_style(px(18), "#FFFFFF", true, "left"), "[서비스 이름]", size_spec("fill", "hug")),
            leaf("dashboard.nav.heading", NodeKind::Text, at(0, 0, 1, 1), text_style(px(11), "#94A3B8", true, "left"), "작업 공간", size_spec("fill", "hug")),
            leaf("dashboard.nav.overview", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#FFFFFF", true, "left"), "개요", size_spec("fill", "hug")),
            leaf("dashboard.nav.reports", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#CBD5E1", false, "left"), "보고서", size_spec("fill", "hug")),
            leaf("dashboard.nav.settings", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#CBD5E1", false, "left"), "설정", size_spec("fill", "hug")),
        ],
    );
    let header = group(
        "dashboard.header",
        at(0, 0, 1, px(40)),
        LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::SpaceBetween), align: "center".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        vec![
            leaf("dashboard.title", NodeKind::Text, at(0, 0, 1, 1), text_style(px(24), "#0F172A", true, "left"), "대시보드", size_spec("hug", "hug")),
            leaf("dashboard.user", NodeKind::Text, at(0, 0, 1, 1), text_style(px(13), "#475569", false, "right"), "[사용자 이름]", size_spec("hug", "hug")),
        ],
    );
    let metrics = container(
        "dashboard.metrics",
        at(0, 0, 1, px(132)),
        plain("transparent"),
        LayoutSpec { mode: "row".into(), gap: px(16), align: "stretch".into(), width_mode: "fill".into(), height_mode: "fixed".into(), ..Default::default() },
        vec![
            dashboard_metric("dashboard.metric.users", "전체 사용자"),
            dashboard_metric("dashboard.metric.activity", "활성 항목"),
            dashboard_metric("dashboard.metric.rate", "완료율"),
        ],
    );
    let chart = container(
        "dashboard.chart",
        at(0, 0, 1, px(280)),
        rounded(bordered("#FFFFFF", "#E2E8F0"), px(8)),
        LayoutSpec {
            mode: "column".into(),
            gap: px(16),
            padding_sides: Some([px(20); 4]),
            align: "start".into(),
            width_mode: "fill".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        vec![
            leaf("dashboard.chart.title", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#0F172A", true, "left"), "활동 추이", size_spec("fill", "hug")),
            container(
                "dashboard.chart.placeholder",
                at(0, 0, 1, px(190)),
                plain("#F8FAFC"),
                LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), width_mode: "fill".into(), height_mode: "fill".into(), ..Default::default() },
                vec![leaf("dashboard.chart.label", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#64748B", false, "center"), "[차트 영역]", size_spec("hug", "hug"))],
            ),
        ],
    );
    let activity = container(
        "dashboard.activity",
        at(0, 0, px(300), px(280)),
        rounded(bordered("#FFFFFF", "#E2E8F0"), px(8)),
        LayoutSpec {
            mode: "column".into(),
            gap: px(14),
            padding_sides: Some([px(20); 4]),
            align: "start".into(),
            width_mode: "fixed".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        vec![
            leaf("dashboard.activity.title", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#0F172A", true, "left"), "최근 활동", size_spec("fill", "hug")),
            leaf("dashboard.activity.1", NodeKind::Text, at(0, 0, 1, 1), text_style(px(13), "#475569", false, "left"), "[활동 항목 1] · [날짜]", size_spec("fill", "hug")),
            leaf("dashboard.activity.2", NodeKind::Text, at(0, 0, 1, 1), text_style(px(13), "#475569", false, "left"), "[활동 항목 2] · [날짜]", size_spec("fill", "hug")),
            leaf("dashboard.activity.3", NodeKind::Text, at(0, 0, 1, 1), text_style(px(13), "#475569", false, "left"), "[활동 항목 3] · [날짜]", size_spec("fill", "hug")),
        ],
    );
    let lower = container(
        "dashboard.lower",
        at(0, 0, 1, 1),
        plain("transparent"),
        LayoutSpec { mode: "row".into(), gap: px(16), align: "stretch".into(), width_mode: "fill".into(), height_mode: "fill".into(), ..Default::default() },
        vec![chart, activity],
    );
    let main = container(
        "dashboard.main",
        at(0, 0, 1, px(900)),
        plain("#F8FAFC"),
        LayoutSpec {
            mode: "column".into(),
            gap: px(20),
            padding_sides: Some([px(28); 4]),
            align: "start".into(),
            width_mode: "fill".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        vec![header, metrics, lower],
    );
    let size = Size { width: px(1440), height: px(900) };
    document(
        "dashboard",
        size.width,
        size.height,
        vec![container(
            "dashboard.page",
            at(0, 0, size.width, size.height),
            plain("#F8FAFC"),
            LayoutSpec { mode: "row".into(), align: "start".into(), width_mode: "fixed".into(), height_mode: "fixed".into(), ..Default::default() },
            vec![sidebar, main],
        )],
    )
}

fn invitation() -> CanvasDocument {
    let size = FRAME_PRESETS.iter().find(|preset| preset.id == "invitation").map_or(
        Size { width: 5 * 914_400, height: 7 * 914_400 },
        |preset| preset.size,
    );
    let card = container(
        "invitation.card",
        at(0, 0, 1, 1),
        bordered("#FFFCF5", "#D6C39A"),
        LayoutSpec {
            mode: "column".into(),
            gap: 5 * MM,
            padding_sides: Some([9 * MM; 4]),
            justify: Some(designllm_contracts::Justify::Center),
            align: "center".into(),
            width_mode: "fill".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        vec![
            leaf("invitation.kicker", NodeKind::Text, at(0, 0, 1, 1), text_style(9 * PT, "#8A6B3F", true, "center"), "INVITATION", size_spec("hug", "hug")),
            leaf(
                "invitation.title",
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { font_size: 25 * PT, font_weight: Some(700), color: "#3B3022".into(), align: "center".into(), ..Style::default() },
                "[행사명]",
                size_spec("fill", "hug"),
            ),
            leaf("invitation.rule", NodeKind::Rectangle, at(0, 0, 1, px(2)), plain("#C7A76C"), "", size_spec("hug", "fixed")),
            leaf(
                "invitation.message",
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { line_height_milli: Some(1500), ..text_style(12 * PT, "#5B4636", false, "center") },
                "[초대 문구를 작성하세요]",
                size_spec("fill", "hug"),
            ),
            leaf(
                "invitation.when",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(12 * PT, "#3B3022", true, "center"),
                "[날짜] · [시간]",
                size_spec("fill", "hug"),
            ),
            leaf(
                "invitation.where",
                NodeKind::Text,
                at(0, 0, 1, 1),
                Style { line_height_milli: Some(1400), ..text_style(10 * PT, "#5B4636", false, "center") },
                "[장소] · [주소]",
                size_spec("fill", "hug"),
            ),
            leaf("invitation.host", NodeKind::Text, at(0, 0, 1, 1), text_style(10 * PT, "#5B4636", false, "center"), "[주최자]", size_spec("hug", "hug")),
            leaf(
                "invitation.rsvp",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(9 * PT, "#64748B", false, "center"),
                "[회신 방법] · [회신 기한]",
                size_spec("fill", "hug"),
            ),
        ],
    );
    let page = container(
        "invitation.page",
        at(0, 0, size.width, size.height),
        plain("#F7F2E8"),
        LayoutSpec {
            mode: "column".into(),
            justify: Some(designllm_contracts::Justify::Center),
            align: "center".into(),
            padding_sides: Some([8 * MM; 4]),
            width_mode: "fixed".into(),
            height_mode: "fixed".into(),
            ..Default::default()
        },
        vec![card],
    );
    let mut doc = document("invitation", size.width, size.height, vec![page]);
    doc.pages[0].name = Some("초대장".into());
    doc
}

fn banner(width: i64, height: i64) -> CanvasDocument {
    let wide = width > height.saturating_mul(2);
    let message = group(
        "banner.message",
        at(0, 0, 1, 1),
        LayoutSpec { mode: "column".into(), gap: px(6), align: "start".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        vec![
            leaf("banner.brand", NodeKind::Text, at(0, 0, 1, 1), text_style(px(11), "#DBEAFE", true, "left"), "[브랜드 이름]", size_spec("fill", "hug")),
            leaf("banner.title", NodeKind::Text, at(0, 0, 1, 1), Style { font_size: if wide { px(18) } else { px(24) }, font_weight: Some(700), color: "#FFFFFF".into(), ..Style::default() }, "[배너 제목]", size_spec("fill", "hug")),
            leaf(
                "banner.subtitle",
                NodeKind::Text,
                at(0, 0, 1, 1),
                text_style(if wide { px(12) } else { px(15) }, "#E2E8F0", false, "left"),
                "[짧은 설명 문구]",
                size_spec("fill", "hug"),
            ),
        ],
    );
    let button = named(
        container(
            "banner.button",
            at(0, 0, if wide { px(120) } else { 1 }, px(48)),
            rounded(plain("#FFFFFF"), px(8)),
            LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), width_mode: if wide { "fixed" } else { "fill" }.into(), height_mode: "fixed".into(), ..Default::default() },
            vec![leaf("banner.button.label", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#1D4ED8", true, "center"), "[행동]", size_spec("hug", "hug"))],
        ),
        "button",
    );
    let (mode, children) = if wide {
        ("row", vec![message, button])
    } else {
        ("column", vec![message, button])
    };
    let page = container(
        "banner.page",
        at(0, 0, width, height),
        graded("#0F172A", "#1E3A8A", 135_000),
        LayoutSpec {
            mode: mode.into(),
            gap: px(16),
            justify: Some(designllm_contracts::Justify::SpaceBetween),
            align: "center".into(),
            padding_sides: Some([if wide { px(12) } else { px(20) }; 4]),
            width_mode: "fixed".into(),
            height_mode: "fixed".into(),
            ..Default::default()
        },
        children,
    );
    document("banner", width, height, vec![page])
}

fn mobile_app(width: i64, height: i64) -> CanvasDocument {
    let card = container(
        "card",
        at(0, 0, 1, 1),
        bordered("#FFFFFF", "#E5E7EB"),
        LayoutSpec {
            mode: "column".into(),
            gap: px(8),
            align: "start".into(),
            padding_sides: Some([px(16); 4]),
            width_mode: "fill".into(),
            height_mode: "hug".into(),
            ..Default::default()
        },
        vec![
            leaf("card.label", NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#111827", true, "left"), "오늘의 할 일", size_spec("fill", "hug")),
            leaf("card.body", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#374151", false, "left"), "[할 일 제목]", size_spec("fill", "hug")),
            named(
                container(
                    "card.button",
                    at(0, 0, px(160), px(48)),
                    rounded(plain("#2563EB"), px(12)),
                    LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), ..Default::default() },
                    vec![leaf("card.button.label", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#FFFFFF", true, "center"), "[확인]", size_spec("hug", "hug"))],
                ),
                "button",
            ),
        ],
    );
    let mut children = vec![
        leaf("title", NodeKind::Text, at(0, 0, 1, 1), text_style(px(22), "#111827", true, "left"), "[앱 이름]", size_spec("fill", "hug")),
        leaf("greeting", NodeKind::Text, at(0, 0, 1, 1), text_style(px(15), "#4B5563", false, "left"), "안녕하세요, [이름]님", size_spec("fill", "hug")),
        card,
        leaf("list.title", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#111827", true, "left"), "최근 항목", size_spec("fill", "hug")),
    ];
    for n in 1..=3 {
        children.push(container(
            &format!("row-{n}"),
            at(0, 0, 1, px(64)),
            bordered("#FFFFFF", "#E5E7EB"),
            LayoutSpec {
                mode: "row".into(),
                gap: px(12),
                align: "center".into(),
                padding_sides: Some([px(16); 4]),
                width_mode: "fill".into(),
                height_mode: "fixed".into(),
                ..Default::default()
            },
            vec![
                leaf(&format!("row-{n}.dot"), NodeKind::Ellipse, px_rect(0, 0, 12, 12), plain("#2563EB"), "", None),
                leaf(&format!("row-{n}.text"), NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#111827", false, "left"), &format!("[항목 {n}]"), size_spec("fill", "hug")),
                icon(&format!("row-{n}.chevron"), "chevron-right", "#9CA3AF", px(16)),
            ],
        ));
    }
    let content = group(
        "content",
        at(0, 0, 1, 1),
        LayoutSpec {
            mode: "column".into(),
            gap: px(16),
            align: "start".into(),
            padding_sides: Some([px(56), px(24), 0, px(24)]),
            width_mode: "fill".into(),
            height_mode: "fill".into(),
            ..Default::default()
        },
        children,
    );
    let tabs: Vec<Node> = [("tab-1", "홈", "home"), ("tab-2", "검색", "search"), ("tab-3", "프로필", "user")]
        .into_iter()
        .map(|(id, label, glyph)| {
            named(
                container(
                    id,
                    at(0, 0, 1, px(56)),
                    plain("#FFFFFF"),
                    LayoutSpec {
                        mode: "column".into(),
                        gap: px(2),
                        justify: Some(designllm_contracts::Justify::Center),
                        align: "center".into(),
                        width_mode: "fill".into(),
                        height_mode: "fixed".into(),
                        ..Default::default()
                    },
                    vec![
                        icon(&format!("{id}.icon"), glyph, "#111827", px(24)),
                        leaf(&format!("{id}.label"), NodeKind::Text, at(0, 0, 1, 1), text_style(px(14), "#111827", true, "center"), label, size_spec("hug", "hug")),
                    ],
                ),
                "tab",
            )
        })
        .collect();
    let tabbar = container(
        "tabbar",
        at(0, 0, 1, px(84)),
        bordered("#FFFFFF", "#E5E7EB"),
        LayoutSpec {
            mode: "row".into(),
            justify: Some(designllm_contracts::Justify::SpaceAround),
            align: "center".into(),
            width_mode: "fill".into(),
            height_mode: "fixed".into(),
            ..Default::default()
        },
        tabs,
    );
    let screen = container(
        "screen",
        at(0, 0, width, height),
        plain("#F4F6FA"),
        LayoutSpec { mode: "column".into(), align: "start".into(), width_mode: "fixed".into(), height_mode: "fixed".into(), ..Default::default() },
        vec![content, tabbar],
    );
    document("mobile-app", width, height, vec![screen])
}

/// Landing page, 1440 × 1600 CSS px. It has a fixed size.
fn landing_page() -> CanvasDocument {
    let nav_links = group(
        "nav.links",
        at(0, 0, 1, 1),
        LayoutSpec { mode: "row".into(), gap: px(32), align: "center".into(), width_mode: "hug".into(), height_mode: "hug".into(), ..Default::default() },
        ["기능", "요금", "문의"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| leaf(&format!("nav.link-{}", i + 1), NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#374151", false, "left"), label, size_spec("hug", "hug")))
            .collect(),
    );
    let hero = container(
        "hero",
        px_rect(0, 72, 1440, 720),
        graded("#EEF2FF", "#DBEAFE", 135_000),
        LayoutSpec { mode: "column".into(), gap: px(24), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), ..Default::default() },
        vec![
            leaf("hero.title", NodeKind::Text, px_rect(0, 0, 1200, 72), Style { letter_spacing: Some(-PX), ..text_style(px(56), "#111827", true, "center") }, "[핵심 가치 제안]", None),
            leaf("hero.sub", NodeKind::Text, px_rect(0, 0, 1000, 32), text_style(px(22), "#374151", false, "center"), "[짧은 설명 문장]", None),
            named(
                container(
                    "hero.cta",
                    px_rect(0, 0, 240, 56),
                    rounded(plain("#2563EB"), px(12)),
                    LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), ..Default::default() },
                    vec![leaf("hero.cta.label", NodeKind::Text, at(0, 0, 1, 1), text_style(px(18), "#FFFFFF", true, "center"), "[시작하기]", size_spec("hug", "hug"))],
                ),
                "button",
            ),
        ],
    );
    let features: Vec<Node> = [("첫 번째 기능", "[기능 설명 문장 1]"), ("두 번째 기능", "[기능 설명 문장 2]"), ("세 번째 기능", "[기능 설명 문장 3]")]
        .into_iter()
        .enumerate()
        .map(|(i, (title, body))| {
            let n = i + 1;
            let glyph = ["check", "chart", "lock"][i];
            container(
                &format!("feature-{n}"),
                px_rect(0, 0, 1, 328),
                Style { shadows: vec![card_shadow()], ..bordered("#F8FAFC", "#E2E8F0") },
                LayoutSpec { mode: "column".into(), gap: px(16), padding_sides: Some([px(32); 4]), align: "start".into(), width_mode: "fill".into(), ..Default::default() },
                vec![
                    container(
                        &format!("feature-{n}.badge"),
                        px_rect(0, 0, 56, 56),
                        rounded(plain("#DBEAFE"), px(28)),
                        LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), ..Default::default() },
                        vec![icon(&format!("feature-{n}.icon"), glyph, "#1D4ED8", px(28))],
                    ),
                    leaf(&format!("feature-{n}.title"), NodeKind::Text, px_rect(0, 0, 1, 36), text_style(px(24), "#111827", true, "left"), title, size_spec("fill", "fixed")),
                    leaf(&format!("feature-{n}.body"), NodeKind::Text, px_rect(0, 0, 1, 96), Style { line_height_milli: Some(1500), ..text_style(px(16), "#374151", false, "left") }, body, size_spec("fill", "fixed")),
                ],
            )
        })
        .collect();
    let features_row = container(
        "features",
        px_rect(0, 792, 1440, 520),
        plain("#FFFFFF"),
        LayoutSpec { mode: "row".into(), gap: px(32), justify: Some(designllm_contracts::Justify::SpaceEvenly), align: "center".into(), padding_sides: Some([px(96), px(160), px(96), px(160)]), ..Default::default() },
        features,
    );
    let footer = container(
        "footer",
        px_rect(0, 1312, 1440, 288),
        plain("#111827"),
        LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), ..Default::default() },
        vec![leaf("footer.text", NodeKind::Text, at(0, 0, 1, 1), text_style(px(16), "#FFFFFF", false, "center"), "[회사명] · [연락처]", size_spec("hug", "hug"))],
    );
    let nav = container(
        "nav",
        px_rect(0, 0, 1440, 72),
        bordered("#FFFFFF", "#E5E7EB"),
        LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::SpaceBetween), align: "center".into(), padding_sides: Some([0, px(48), 0, px(48)]), ..Default::default() },
        vec![
            leaf("nav.brand", NodeKind::Text, at(0, 0, 1, 1), text_style(px(20), "#111827", true, "left"), "[브랜드]", size_spec("hug", "hug")),
            nav_links,
        ],
    );
    let page_bg = leaf("page-bg", NodeKind::Rectangle, px_rect(0, 0, 1440, 1600), plain("#FFFFFF"), "", None);
    document("landing-page", px(1440), px(1600), vec![page_bg, nav, hero, features_row, footer])
}

/// Social post at `width` × `height` EMU: the background fills the page, the headline stacks from the top and the handle sits at
/// the bottom. The offered sizes are 1080 × 1080, 1080 × 1350 and 1080 × 1920 CSS px.
fn social_post(width: i64, height: i64) -> CanvasDocument {
    let top = group(
        "top",
        at(0, 0, 1, 1),
        LayoutSpec { mode: "column".into(), gap: px(24), align: "start".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        vec![
            leaf("accent", NodeKind::Rectangle, px_rect(0, 0, 160, 12), plain("#F59E0B"), "", None),
            leaf("title", NodeKind::Text, at(0, 0, 1, 1), Style { letter_spacing: Some(-PX), ..text_style(px(88), "#FFFFFF", true, "left") }, "[캠페인 제목]", size_spec("fill", "hug")),
            leaf("subtitle", NodeKind::Text, at(0, 0, 1, 1), text_style(px(40), "#CBD5E1", false, "left"), "[부제]", size_spec("fill", "hug")),
        ],
    );
    let handle = leaf("handle", NodeKind::Text, at(0, 0, 1, 1), text_style(px(32), "#E2E8F0", false, "left"), "[계정 이름]", size_spec("hug", "hug"));
    let handle_row = group(
        "handle-row",
        at(0, 0, 1, 1),
        LayoutSpec { mode: "row".into(), gap: px(12), align: "center".into(), width_mode: "hug".into(), height_mode: "hug".into(), ..Default::default() },
        vec![icon("handle.icon", "heart", "#F472B6", px(32)), handle],
    );
    let bg = container(
        "bg",
        at(0, 0, width, height),
        graded("#0F172A", "#312E81", 135_000),
        LayoutSpec {
            mode: "column".into(),
            justify: Some(designllm_contracts::Justify::SpaceBetween),
            align: "start".into(),
            padding_sides: Some([px(96); 4]),
            width_mode: "fixed".into(),
            height_mode: "fixed".into(),
            ..Default::default()
        },
        vec![top, handle_row],
    );
    document("social-post", width, height, vec![bg])
}

/// Poster at `width` × `height` EMU: the headline at the top, the hero taking the space between, and the facts row at the bottom,
/// inside a 20 mm margin. The offered sizes are A3 and A4.
fn poster_a3(width: i64, height: i64) -> CanvasDocument {
    let head = group(
        "head",
        at(0, 0, 1, 1),
        LayoutSpec { mode: "column".into(), gap: 8 * MM, align: "start".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        vec![
            leaf("head.title", NodeKind::Text, at(0, 0, 1, 1), text_style(72 * PT, "#7C2D12", true, "left"), "[이벤트 이름]", size_spec("fill", "hug")),
            leaf("head.date", NodeKind::Text, at(0, 0, 1, 1), text_style(36 * PT, "#9A3412", false, "left"), "[날짜] · [시간]", size_spec("fill", "hug")),
        ],
    );
    let hero = container(
        "hero",
        at(0, 0, 1, 1),
        rounded(graded("#FDBA74", "#FB923C", 90_000), 8 * MM),
        LayoutSpec { mode: "column".into(), justify: Some(designllm_contracts::Justify::Center), align: "center".into(), width_mode: "fill".into(), height_mode: "fill".into(), ..Default::default() },
        vec![leaf("hero.message", NodeKind::Text, at(0, 0, 1, 1), text_style(96 * PT, "#431407", true, "center"), "[핵심 메시지]", size_spec("fill", "hug"))],
    );
    let glyphs = ["map-pin", "info", "phone"];
    let items: Vec<Node> = ["[장소]", "[주최]", "[문의 번호]"]
        .into_iter()
        .zip(glyphs)
        .enumerate()
        .map(|(i, (label, glyph))| {
            group(
                &format!("info.{}", i + 1),
                at(0, 0, 1, 1),
                LayoutSpec { mode: "row".into(), gap: 4 * MM, align: "center".into(), width_mode: "hug".into(), height_mode: "hug".into(), ..Default::default() },
                vec![
                    icon(&format!("info.{}.icon", i + 1), glyph, "#7C2D12", 10 * MM),
                    leaf(&format!("info.{}.label", i + 1), NodeKind::Text, at(0, 0, 1, 1), text_style(32 * PT, "#7C2D12", false, "left"), label, size_spec("hug", "hug")),
                ],
            )
        })
        .collect();
    let info = group(
        "info",
        at(0, 0, 1, 1),
        LayoutSpec { mode: "row".into(), justify: Some(designllm_contracts::Justify::SpaceBetween), align: "center".into(), width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() },
        items,
    );
    let bg = container(
        "bg",
        at(0, 0, width, height),
        plain("#FDF6EC"),
        LayoutSpec {
            mode: "column".into(),
            justify: Some(designllm_contracts::Justify::SpaceBetween),
            align: "start".into(),
            padding_sides: Some([20 * MM; 4]),
            width_mode: "fixed".into(),
            height_mode: "fixed".into(),
            ..Default::default()
        },
        vec![head, hero, info],
    );
    document("poster-a3", width, height, vec![bg])
}

/// The title of each template: its home tile's (crates/desktop/src/app/home.rs `TILES`). The landing page has no home tile, so
/// it takes the title of its design tile (design/gui-v2/home-templates.html).
const TITLES: &[(&str, &str)] = &[
    ("mobile_app", "모바일 앱"),
    ("landing_page", "랜딩 페이지"),
    ("social_post", "소셜 게시물"),
    ("poster_a3", "포스터 · 전단"),
    ("resume", "이력서"),
    ("login", "로그인"),
    ("dashboard", "대시보드"),
    ("invitation", "초대장"),
    ("banner", "배너"),
];

/// Every template's page size when `create` names none.
pub(crate) fn default_size(template: &str) -> Option<Size> {
    match template {
        "mobile_app" => Some(Size { width: px(390), height: px(844) }),
        "landing_page" => Some(Size { width: px(1440), height: px(1600) }),
        "social_post" => Some(Size { width: px(1080), height: px(1080) }),
        "poster_a3" => Some(Size { width: 297 * MM, height: 420 * MM }),
        "resume" => Some(Size::a4()),
        "login" => Some(Size { width: px(390), height: px(844) }),
        "dashboard" => Some(Size { width: px(1440), height: px(900) }),
        "invitation" => FRAME_PRESETS.iter().find(|preset| preset.id == "invitation").map(|preset| preset.size),
        "banner" => FRAME_PRESETS.iter().find(|preset| preset.id == "banner_medium").map(|preset| preset.size),
        _ => None,
    }
}

/// The frame presets a template is offered at (GUI_DESIGN §3), default first. A template with none keeps its own fixed size.
pub(crate) fn offered_presets(template: &str) -> &'static [&'static str] {
    match template {
        "mobile_app" => &["phone", "phone_large"],
        "social_post" => &["social_square", "social_portrait", "story"],
        "poster_a3" => &["a3", "a4"],
        "resume" => &["a4"],
        "login" => &["phone", "phone_large"],
        "dashboard" => &["desktop"],
        "invitation" => &["invitation"],
        "banner" => &["banner_medium", "leaderboard"],
        _ => &[],
    }
}

/// Whether a template lays out for any size it is given. The others keep their fixed size.
pub(crate) fn is_responsive(template: &str) -> bool {
    matches!(template, "mobile_app" | "social_post" | "poster_a3" | "login" | "banner")
}

/// What `EngineAdapter::templates` reports, in `TEMPLATES` order: one entry per offered frame preset (the preset and its size),
/// or one entry at the template's own size when it has none. `create` accepts each entry's size as the page size.
pub(crate) fn catalog() -> Vec<TemplateInfo> {
    TEMPLATES
        .iter()
        .flat_map(|name| {
            let title = TITLES.iter().find(|(id, _)| id == name).map_or("", |(_, title)| *title);
            let entry = |preset: Option<&str>, size: Size| TemplateInfo {
                id: (*name).into(),
                kind: ArtifactKind::Canvas,
                title: title.into(),
                preset: preset.map(str::to_string),
                size: Some(size),
            };
            let presets = offered_presets(name);
            if presets.is_empty() {
                default_size(name).map(|size| entry(None, size)).into_iter().collect::<Vec<_>>()
            } else {
                presets
                    .iter()
                    .filter_map(|id| FRAME_PRESETS.iter().find(|preset| preset.id == *id).map(|preset| entry(Some(preset.id), preset.size)))
                    .collect()
            }
        })
        .collect()
}

/// The document a template builds at `size`, or None for an unknown name. The responsive templates lay out for the size; the
/// landing page is always its own size (the caller refuses another).
pub(crate) fn build(template: &str, size: Size) -> Option<CanvasDocument> {
    Some(match template {
        "mobile_app" => mobile_app(size.width, size.height),
        "landing_page" => landing_page(),
        "social_post" => social_post(size.width, size.height),
        "poster_a3" => poster_a3(size.width, size.height),
        "resume" => resume(),
        "login" => login(size.width, size.height),
        "dashboard" => dashboard(),
        "invitation" => invitation(),
        "banner" => banner(size.width, size.height),
        _ => return None,
    })
}

fn channel(hex: &str, at: usize) -> Option<f64> {
    let v = u8::from_str_radix(hex.get(at..at + 2)?, 16).ok()?;
    let s = f64::from(v) / 255.0;
    Some(if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) })
}

/// Relative luminance of `#RRGGBB` or `#RRGGBBAA` (alpha ignored). None for anything else.
fn luminance(color: &str) -> Option<f64> {
    let hex = color.strip_prefix('#').filter(|h| h.is_ascii() && (h.len() == 6 || h.len() == 8))?;
    Some(0.2126 * channel(hex, 0)? + 0.7152 * channel(hex, 2)? + 0.0722 * channel(hex, 4)?)
}

/// WCAG contrast ratio of two colours, or None when one is not `#RRGGBB`.
pub(crate) fn contrast(a: &str, b: &str) -> Option<f64> {
    let (la, lb) = (luminance(a)?, luminance(b)?);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    Some((hi + 0.05) / (lo + 0.05))
}

fn diagnostic(code: &str, message: String, node_id: &str) -> Diagnostic {
    Diagnostic { code: code.into(), message, node_id: Some(node_id.into()), blocking: true, params: Default::default() }
}

/// The colours a shape paints: its fill, or both stops of its gradient.
fn painted_colours(style: &Style) -> Vec<String> {
    match &style.gradient {
        Some(gradient) => gradient.stops.iter().map(|stop| stop.color.clone()).collect(),
        None => vec![style.fill.clone()],
    }
}

/// The content box of a frame: the frame less its padding (per side when set, else the uniform padding).
fn inner_frame(outer: Rect, layout: &LayoutSpec) -> Rect {
    let [top, right, bottom, left] = layout.padding_sides.unwrap_or([layout.padding; 4]);
    Rect { x: outer.x + left, y: outer.y + top, width: outer.width - left - right, height: outer.height - top - bottom }
}

/// Whether `inner` lies inside `outer`, edges included.
fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// Template rules over a document, its scene, and the frame of every node drawn (`frames`, by node ID). Empty means the
/// document passes. The codes are `CONTRAST_LOW`, `TOUCH_TARGET_SMALL`, `TEXT_CLIPPED`, `OUT_OF_PAGE`, `CHILD_OUTSIDE_PARENT`
/// and `LOCKED_NODE`.
pub(crate) fn lint(doc: &CanvasDocument, scene: &RenderScene, frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let page_background = "#FFFFFF";
    for page in &scene.pages {
        // The shape each node was drawn with, and the text lines in draw order.
        let mut shapes: Vec<(&str, Rect, Vec<String>)> = Vec::new();
        for primitive in &page.primitives {
            match primitive {
                Primitive::Shape { node_id, bounds, style, .. } => shapes.push((node_id, *bounds, painted_colours(style))),
                Primitive::Text { node_id, bounds, .. } => {
                    let Some(color) = find_color(doc, node_id) else {
                        continue;
                    };
                    let (cx, cy) = (bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
                    // The last shape drawn under the line's centre is what the text sits on. A gradient counts every stop.
                    let background: Vec<String> = shapes
                        .iter()
                        .rev()
                        .find(|(_, r, fills)| {
                            !fills.iter().any(|f| f.as_str() == "transparent") && cx >= r.x && cx < r.x + r.width && cy >= r.y && cy < r.y + r.height
                        })
                        .map_or_else(|| vec![page_background.to_string()], |(_, _, fills)| fills.clone());
                    // The lowest contrast over those colours is the one the text must meet.
                    let worst = background
                        .iter()
                        .filter_map(|bg| contrast(color, bg).map(|ratio| (ratio, bg.as_str())))
                        .min_by(|a, b| a.0.total_cmp(&b.0));
                    if let Some((ratio, bg)) = worst.filter(|(ratio, _)| *ratio < CONTRAST_MIN) {
                        out.push(diagnostic(
                            "CONTRAST_LOW",
                            format!("text colour {color} on {bg} is {ratio:.2}:1; at least {CONTRAST_MIN:.1}:1 is needed"),
                            node_id,
                        )
                        .with_param("ratio", (ratio * 10.0).round() / 10.0)
                        .with_param("min", CONTRAST_MIN));
                    }
                }
                _ => {}
            }
        }
    }
    for node in designllm_contracts_flatten(doc) {
        if node.ext.locked == Some(true) {
            out.push(diagnostic("LOCKED_NODE", format!("node {} is locked; templates must stay editable", node.id), &node.id));
        }
        // Interactive nodes: named as a button or a tab, or carrying an interaction.
        let interactive = matches!(node.ext.name.as_deref(), Some("button") | Some("tab")) || !node.interactions.is_empty();
        if interactive {
            if let Some(bounds) = frames.get(&node.id).copied().or_else(|| scene_bounds(scene, &node.id)) {
                let min = TOUCH_MIN_PX * PX;
                if bounds.width < min || bounds.height < min {
                    out.push(diagnostic(
                        "TOUCH_TARGET_SMALL",
                        format!(
                            "touch target {} is {}x{} px; at least {TOUCH_MIN_PX}x{TOUCH_MIN_PX} px is needed",
                            node.id,
                            bounds.width / PX,
                            bounds.height / PX
                        ),
                        &node.id,
                    )
                    .with_param("w", bounds.width / PX)
                    .with_param("h", bounds.height / PX)
                    .with_param("min", TOUCH_MIN_PX));
                }
            }
        }
    }
    for finding in &scene.diagnostics {
        if finding.code == "TEXT_OVERFLOW" {
            out.push(Diagnostic {
                code: "TEXT_CLIPPED".into(),
                message: finding.message.clone(),
                node_id: finding.node_id.clone(),
                blocking: true,
                params: finding.params.clone(),
            });
        }
    }
    // Nothing is drawn outside its page, and a child stays inside the frame of its parent.
    for page in &doc.pages {
        let sheet = Rect { x: 0, y: 0, width: page.size.width, height: page.size.height };
        for node in flatten_nodes(&page.nodes) {
            if let Some(frame) = frames.get(&node.id) {
                if !contains(sheet, *frame) {
                    out.push(diagnostic("OUT_OF_PAGE", format!("node {} is drawn outside the page", node.id), &node.id).with_param("node", node.id.clone()));
                }
            }
        }
    }
    for node in designllm_contracts_flatten(doc) {
        let Some(outer) = frames.get(&node.id) else {
            continue;
        };
        // A child of a container with a layout stays inside the container's padding.
        let room = node.layout.as_ref().map_or(*outer, |layout| inner_frame(*outer, layout));
        for child in &node.children {
            if let Some(inner) = frames.get(&child.id) {
                if !contains(room, *inner) {
                    out.push(diagnostic(
                        "CHILD_OUTSIDE_PARENT",
                        format!("node {} is drawn outside its parent {}", child.id, node.id),
                        &child.id,
                    )
                    .with_param("node", child.id.clone())
                    .with_param("parent", node.id.clone()));
                }
            }
        }
    }
    out
}

/// The template rules as warnings on a user's document, after layout. They are non-blocking, so an edit is never refused by them.
/// `LOCKED_NODE` is not a design rule and is left out.
pub(crate) fn design_warnings(doc: &CanvasDocument, scene: &RenderScene, frames: &BTreeMap<String, Rect>) -> Vec<Diagnostic> {
    lint(doc, scene, frames)
        .into_iter()
        .filter(|finding| finding.code != "LOCKED_NODE")
        .map(|finding| Diagnostic { blocking: false, ..finding })
        .collect()
}

fn designllm_contracts_flatten(doc: &CanvasDocument) -> Vec<&Node> {
    doc.pages.iter().flat_map(|page| flatten_nodes(&page.nodes)).collect()
}

fn flatten_nodes(nodes: &[Node]) -> Vec<&Node> {
    let mut out = Vec::new();
    for node in nodes {
        out.push(node);
        out.extend(flatten_nodes(&node.children));
    }
    out
}

fn find_color<'a>(doc: &'a CanvasDocument, id: &str) -> Option<&'a str> {
    designllm_contracts_flatten(doc).into_iter().find(|n| n.id == id).map(|n| n.style.color.as_str())
}

fn scene_bounds(scene: &RenderScene, id: &str) -> Option<Rect> {
    scene.pages.iter().flat_map(|page| page.primitives.iter()).find_map(|primitive| match primitive {
        Primitive::Shape { node_id, bounds, .. } if node_id == id => Some(*bounds),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_icon_is_one_path_on_the_24_grid() {
        for (name, _) in ICONS {
            let node = icon("probe", name, "#000000", px(24));
            let path = node.ext.path.expect("a path");
            assert_eq!(path.view_box, at(0, 0, 24_000, 24_000), "{name}");
            assert_eq!(node.ext.icon.expect("an icon").name, *name);
        }
    }

    #[test]
    fn contrast_follows_the_wcag_formula() {
        // Black on white is the maximum, 21:1.
        assert!((contrast("#000000", "#FFFFFF").expect("ratio") - 21.0).abs() < 1e-9);
        // The mobile button label on its fill passes 4.5:1.
        assert!(contrast("#FFFFFF", "#2563EB").expect("ratio") >= CONTRAST_MIN);
        // Light grey on white fails.
        assert!(contrast("#CCCCCC", "#FFFFFF").expect("ratio") < CONTRAST_MIN);
        assert_eq!(contrast("red", "#FFFFFF"), None);
    }

    #[test]
    fn every_template_name_builds_at_its_default_size_and_unknown_names_do_not() {
        for name in TEMPLATES {
            let size = default_size(name).expect("a default size");
            assert!(build(name, size).is_some(), "{name}");
        }
        assert!(build("nope", Size { width: px(390), height: px(844) }).is_none());
    }

    #[test]
    fn findings_carry_the_measured_values_as_params() {
        // The button is drawn as a shape, which is how the touch rule finds its size.
        let button_shape = Primitive::Shape { node_id: "btn".into(), bounds: px_rect(0, 0, 20, 20), shape: "rect".into(), style: plain("#2563EB") };
        let page = designllm_contracts::RenderPage {
            id: "p".into(),
            page_index: 0,
            size: Size { width: px(200), height: px(200) },
            primitives: vec![button_shape],
            source_map: vec![],
        };
        let scene = RenderScene { pages: vec![page], font_fingerprint: String::new(), diagnostics: vec![], metadata: BTreeMap::new(), links: vec![] };
        // A 20 × 20 px button: the touch rule measures it in CSS px.
        let mut button = leaf("btn", NodeKind::Rectangle, px_rect(0, 0, 20, 20), plain("#2563EB"), "", None);
        button.ext.name = Some("button".into());
        let doc = document("p", px(200), px(200), vec![button]);
        let mut frames = BTreeMap::new();
        frames.insert("btn".to_string(), px_rect(0, 0, 20, 20));
        let touch = lint(&doc, &scene, &frames).into_iter().find(|d| d.code == "TOUCH_TARGET_SMALL").expect("the touch rule");
        assert_eq!(touch.params.get("w"), Some(&serde_json::json!(20)));
        assert_eq!(touch.params.get("h"), Some(&serde_json::json!(20)));
        assert_eq!(touch.params.get("min"), Some(&serde_json::json!(44)));
        assert!(touch.message.ends_with("at least 44x44 px is needed"), "{}", touch.message);
        // A child that starts in its parent's padding lies outside the parent's content box.
        let outer = container(
            "outer",
            px_rect(0, 0, 100, 100),
            plain("#FFFFFF"),
            LayoutSpec { mode: "column".into(), padding_sides: Some([px(10); 4]), ..Default::default() },
            vec![leaf("inner", NodeKind::Rectangle, px_rect(0, 0, 20, 20), plain("#2563EB"), "", None)],
        );
        let doc = document("p", px(200), px(200), vec![outer]);
        let mut frames = BTreeMap::new();
        frames.insert("outer".to_string(), px_rect(0, 0, 100, 100));
        frames.insert("inner".to_string(), px_rect(0, 0, 20, 20));
        let child = lint(&doc, &scene, &frames).into_iter().find(|d| d.code == "CHILD_OUTSIDE_PARENT").expect("the child rule");
        assert_eq!(child.params.get("node"), Some(&serde_json::json!("inner")));
        assert_eq!(child.params.get("parent"), Some(&serde_json::json!("outer")));
    }

    #[test]
    fn the_contrast_finding_names_the_minimum_it_checks() {
        // White text on the white page is 1:1, below the 4.5:1 minimum.
        let shaped = designllm_contracts::ShapedText {
            size: Size { width: px(40), height: px(12) },
            baseline: 0,
            ascent: 0,
            descent: 0,
            line_gap: 0,
            cap_height: 0,
            glyphs: vec![],
            caret_x: vec![],
            grapheme_rects: vec![],
            fonts_used: vec![],
            missing_glyphs: vec![],
        };
        let text_primitive = Primitive::Text {
            node_id: "t".into(),
            bounds: px_rect(0, 0, 40, 12),
            origin: designllm_contracts::Point { x: 0, y: 0 },
            text: "Hi".into(),
            style: text_style(px(12), "#FFFFFF", false, "left"),
            shaped,
        };
        let page = designllm_contracts::RenderPage {
            id: "p".into(),
            page_index: 0,
            size: Size { width: px(200), height: px(200) },
            primitives: vec![text_primitive],
            source_map: vec![],
        };
        let scene = RenderScene { pages: vec![page], font_fingerprint: String::new(), diagnostics: vec![], metadata: BTreeMap::new(), links: vec![] };
        let doc = document("p", px(200), px(200), vec![leaf("t", NodeKind::Text, px_rect(0, 0, 40, 12), text_style(px(12), "#FFFFFF", false, "left"), "Hi", None)]);
        let frames = BTreeMap::new();
        let contrast = lint(&doc, &scene, &frames).into_iter().find(|d| d.code == "CONTRAST_LOW").expect("the contrast rule");
        assert_eq!(contrast.message, "text colour #FFFFFF on #FFFFFF is 1.00:1; at least 4.5:1 is needed");
        assert_eq!(contrast.params.get("min"), Some(&serde_json::json!(4.5)));
        assert_eq!(contrast.params.get("min"), Some(&serde_json::json!(CONTRAST_MIN)));
    }

    #[test]
    fn the_catalog_offers_each_preset_at_its_size_and_the_fixed_template_once() {
        let ids: Vec<(String, Option<String>)> = catalog().into_iter().map(|info| (info.id, info.preset)).collect();
        let expected: Vec<(&str, Option<&str>)> = vec![
            ("mobile_app", Some("phone")),
            ("mobile_app", Some("phone_large")),
            ("landing_page", None),
            ("social_post", Some("social_square")),
            ("social_post", Some("social_portrait")),
            ("social_post", Some("story")),
            ("poster_a3", Some("a3")),
            ("poster_a3", Some("a4")),
            ("resume", Some("a4")),
            ("login", Some("phone")),
            ("login", Some("phone_large")),
            ("dashboard", Some("desktop")),
            ("invitation", Some("invitation")),
            ("banner", Some("banner_medium")),
            ("banner", Some("leaderboard")),
        ];
        let expected: Vec<(String, Option<String>)> = expected.into_iter().map(|(id, preset)| (id.to_string(), preset.map(str::to_string))).collect();
        assert_eq!(ids, expected);
    }
}
