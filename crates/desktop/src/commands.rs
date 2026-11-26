//! The command registry (U10, U11). Every button, menu item and shortcut of the shell is one named
//! command with an id, a JSON argument schema and a permission class. Widgets only invoke commands
//! (`DesktopApp::invoke`); the same table feeds the keyboard shortcuts and, once the core's UI
//! channel lands, `ui.command.list` / `ui.command.run`.

use eframe::egui::{Key, KeyboardShortcut, Modifiers};
use serde_json::Value;
use std::collections::BTreeSet;
use designllm_contracts::{ArtifactKind, Intent, UiPermission};

/// What a command may do (U10, U11-4). `Read` changes only what is shown; `Design` and `Content`
/// change the document; `FileWrite` writes a file, which the MCP rules gate in the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Read,
    Design,
    Content,
    /// Changes a figure's data (a dataset or the columns a plot reads).
    Data,
    FileWrite,
}

#[derive(Clone, Copy, Debug)]
pub struct CommandSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub permission: Permission,
    /// `Ctrl+Shift+S` style, or empty.
    pub shortcut: &'static str,
    /// A JSON Schema object for the arguments, as text.
    pub args: &'static str,
}

const NONE: &str = r#"{"type":"object","properties":{}}"#;
const NODE: &str = r#"{"type":"object","required":["node_id"],"properties":{"node_id":{"type":"string"}}}"#;
const NUMBER_PX: &str = r#"{"type":"object","required":["value"],"properties":{"value":{"type":"number"}}}"#;
const COLOR: &str = r#"{"type":"object","required":["color"],"properties":{"color":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"}}}"#;
const XY: &str = r#"{"type":"object","required":["x","y"],"properties":{"x":{"type":"number"},"y":{"type":"number"}}}"#;
const POSITION: &str = r#"{"type":"object","properties":{"x":{"type":"number"},"y":{"type":"number"}}}"#;
const SIZE: &str = r#"{"type":"object","properties":{"width":{"type":"number"},"height":{"type":"number"}}}"#;
const PERCENT: &str = r#"{"type":"object","required":["percent"],"properties":{"percent":{"type":"number","minimum":0,"maximum":100}}}"#;
const ALIGN: &str = r#"{"type":"object","required":["edge"],"properties":{"edge":{"type":"string","enum":["left","h-center","right","top","v-center","bottom"]}}}"#;
const PAGE_INDEX: &str = r#"{"type":"object","required":["page_index"],"properties":{"page_index":{"type":"integer","minimum":0}}}"#;
const PAGE_COMPONENT: &str = r#"{"type":"object","required":["enabled"],"properties":{"enabled":{"type":"boolean"},"page_index":{"type":"integer","minimum":0}}}"#;
const COMPONENT_INSTANTIATE: &str = r#"{"type":"object","required":["component_page_id","instance_id"],"properties":{"component_page_id":{"type":"string","minLength":1},"instance_id":{"type":"string","minLength":1},"page_index":{"type":"integer","minimum":0},"parent_id":{"type":"string"},"bounds":{"type":"object","required":["x","y","width","height"],"description":"EMU in the target page or parent coordinate space","properties":{"x":{"type":"integer","description":"EMU"},"y":{"type":"integer","description":"EMU"},"width":{"type":"integer","minimum":1,"description":"EMU"},"height":{"type":"integer","minimum":1,"description":"EMU"}}}}}"#;
const PRESET: &str = r#"{"type":"object","properties":{"preset":{"type":"string"}}}"#;
const PROMPT: &str = r#"{"type":"object","required":["prompt"],"properties":{"prompt":{"type":"string"}}}"#;
const ARTIFACT: &str = r#"{"type":"object","required":["artifact_id"],"properties":{"artifact_id":{"type":"string"}}}"#;
const BOOL: &str = r#"{"type":"object","required":["on"],"properties":{"on":{"type":"boolean"}}}"#;
const TEXT_LIST: &str = r#"{"type":"object","required":["list"],"properties":{"list":{"type":"string","enum":["bullet","numbered","none"]}}}"#;
const TAB: &str = r#"{"type":"object","required":["tab"],"properties":{"tab":{"type":"string","enum":["layers","tools","ai","outline","files","projects","design_systems","templates"]}}}"#;
const FILE_EXPORT: &str = r#"{"type":"object","required":["format","target_path"],"properties":{"format":{"type":"string","enum":["png","jpeg","svg","pdf","html","pptx","zip"]},"target_path":{"type":"string"},"overwrite":{"type":"boolean"},"expected_target_hash":{"type":"string"},"allowed_losses":{"type":"array","items":{"type":"string"}},"scale":{"type":"integer","enum":[1,2,3]},"dpi":{"type":"integer","minimum":72,"maximum":1200},"background":{"type":"string","enum":["page","transparent"]},"scope":{"type":"string","enum":["page","selection","all","range"]},"pages":{"type":"string","description":"scope가 range일 때 내보낼 쪽 번호. 1부터 세고, 쉼표로 나열하며 a-b로 범위를 씁니다. 예: 1,3-5"}}}"#;
const FILE_SAVE: &str = r#"{"type":"object","required":["target_path"],"properties":{"target_path":{"type":"string"},"overwrite":{"type":"boolean"},"expected_target_hash":{"type":"string"}}}"#;
const AI_CANCEL: &str = r#"{"type":"object","required":["job_id","attempt_id"],"properties":{"job_id":{"type":"string"},"attempt_id":{"type":"string"}}}"#;
const PROPOSAL_ID: &str = r#"{"type":"object","required":["proposal_id"],"properties":{"proposal_id":{"type":"string","minLength":1}}}"#;
const CREATE: &str = r#"{"type":"object","required":["tile"],"description":"tile로 작업물 종류를 고릅니다. 크기와 실행기 설정은 생략하면 현재 홈의 값을 사용합니다.","properties":{"tile":{"type":"string","enum":["blank","mobile","ui","wireframe","slides","document","resume","poster","social","figure","diagram","web","animation"]},"prompt":{"type":"string"},"preset":{"type":"string"},"runtime":{"type":"string","enum":["claude","agy","codex"]},"model":{"type":"string"},"effort":{"type":"string","enum":["minimal","low","medium","high","xhigh","max"]},"mode":{"type":"string","enum":["design","code"],"description":"홈 탭 표시 모드. 생성할 작업물은 tile이 결정합니다."}}}"#;
/// The optional placement of an inserted object (68g): `at` names the page and the object's centre in the document's units
/// (px for canvas and slides, mm for text documents), measured from the page's top left. Without it the object goes to the
/// fixed place of the page. An `at` outside the page is refused.
const ARG_AT: &str = r#"{"type":"object","properties":{"at":{"type":"object","description":"삽입할 위치. 개체의 가운데 점을 쪽의 왼쪽 위에서 잰 x, y(캔버스·슬라이드는 px)로 줍니다. page_index를 생략하면 현재 쪽입니다. 쪽 밖이면 거부합니다.","required":["x","y"],"properties":{"page_index":{"type":"integer","minimum":0},"x":{"type":"number"},"y":{"type":"number"}}}}}"#;
const ARG_ICON: &str = r##"{"type":"object","required":["icon"],"properties":{"icon":{"type":"string"},"size":{"type":"number","minimum":8},"color":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"at":{"type":"object","description":"삽입할 위치(add.text와 같은 뜻). 아이콘의 가운데 점을 줍니다.","required":["x","y"],"properties":{"page_index":{"type":"integer","minimum":0},"x":{"type":"number"},"y":{"type":"number"}}}}}"##;
const ARG_PATH: &str = r#"{"type":"object","required":["d","view_box"],"properties":{"d":{"type":"string"},"view_box":{"type":"string"}}}"#;
const ARG_NODE_PATH: &str = r#"{"type":"object","required":["d","view_box"],"properties":{"d":{"type":"string"},"view_box":{"type":"string"},"node_id":{"type":"string"}}}"#;
const TOKEN_PUT_ARGS: &str = r##"{"type":"object","required":["token_id","name","values"],"description":"선택한 캔버스 작업물의 로컬 디자인 토큰을 추가하거나 갱신합니다.","properties":{"token_id":{"type":"string","minLength":1},"name":{"type":"string","minLength":1},"values":{"type":"object","description":"font_size, spacing, radius의 숫자 값은 정수 EMU입니다. 예: 16 CSS px = 152400 EMU. GUI의 px 입력은 한 번만 EMU로 변환됩니다.","oneOf":[{"type":"object","required":["kind","light","dark"],"properties":{"kind":{"type":"string","enum":["color"]},"light":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"dark":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"}},"additionalProperties":false},{"type":"object","required":["kind","light","dark"],"properties":{"kind":{"type":"string","enum":["font_family"]},"light":{"type":"string","minLength":1},"dark":{"type":"string","minLength":1}},"additionalProperties":false},{"type":"object","required":["kind","light","dark"],"properties":{"kind":{"type":"string","enum":["font_size","spacing","radius"]},"light":{"type":"integer"},"dark":{"type":"integer"}},"additionalProperties":false}]}}}"##;
const TOKEN_BIND_ARGS: &str = r##"{"type":"object","required":["token_id","target"],"description":"현재 GUI 선택에 로컬 또는 연결된 공유 토큰을 연결합니다. source는 local(기본값) 또는 shared입니다. MCP에서는 먼저 select.node 또는 select.many를 실행하세요. layout_padding_side에는 side가 필요하고 breakpoint_gap/padding에는 min_width_css_px가 필요합니다.","properties":{"token_id":{"type":"string","minLength":1},"source":{"type":"string","enum":["local","shared"]},"target":{"type":"string","enum":["style_color","style_fill","style_stroke","style_font_family","style_font_size","style_corner_radius","layout_gap","layout_row_gap","layout_padding","layout_padding_side","breakpoint_gap","breakpoint_padding"]},"side":{"type":"string","enum":["top","right","bottom","left"]},"min_width_css_px":{"type":"integer","minimum":1}}}"##;
const DESIGN_SYSTEM_SET_ARGS: &str = r##"{"type":"object","required":["design_system"],"description":"현재 Canvas, WebCode, Word, HWPX 작업물을 공유 디자인 시스템에 연결하거나 null로 연결 해제합니다. 연결 시 expected_system_revision을 전달하면 원본이 바뀐 경우 Core가 거부합니다. Word와 HWPX 연결에는 direct_formatting이 필요합니다: keep은 직접 서식을 그대로 두고, clear는 그 스타일을 쓰는 글자의 직접 서식을 지웁니다(HWPX는 keep만).","properties":{"design_system":{"oneOf":[{"type":"string","minLength":1},{"type":"null"}]},"expected_system_revision":{"type":"integer","minimum":0},"direct_formatting":{"type":"string","enum":["keep","clear"]}}}"##;
const TOKEN_UNBIND_ARGS: &str = r##"{"type":"object","required":["target"],"description":"현재 GUI 선택에서 지정한 로컬 토큰 대상을 연결 해제합니다. MCP에서는 먼저 select.node 또는 select.many를 실행하세요. layout_padding_side에는 side가 필요하고 breakpoint_gap/padding에는 min_width_css_px가 필요합니다.","properties":{"target":{"type":"string","enum":["style_color","style_fill","style_stroke","style_font_family","style_font_size","style_corner_radius","layout_gap","layout_row_gap","layout_padding","layout_padding_side","breakpoint_gap","breakpoint_padding"]},"side":{"type":"string","enum":["top","right","bottom","left"]},"min_width_css_px":{"type":"integer","minimum":1}}}"##;
const ORDER: &str = NONE;
const GUIDES_SET: &str = r#"{"type":"object","required":["axis","count","spacing_emu","leading_margin_emu","trailing_margin_emu"],"description":"캔버스 페이지의 열 또는 행 안내선 한 축을 설정합니다. 간격과 여백은 EMU 정수이며 0 이상이고, 모든 트랙은 최소 1 EMU를 가져야 합니다. 다른 축은 그대로 둡니다.","properties":{"page_index":{"type":"integer","minimum":0},"axis":{"type":"string","enum":["columns","rows"]},"count":{"type":"integer","minimum":1,"maximum":24},"spacing_emu":{"type":"integer","minimum":0},"leading_margin_emu":{"type":"integer","minimum":0},"trailing_margin_emu":{"type":"integer","minimum":0}}}"#;
const GUIDES_CLEAR: &str = r#"{"type":"object","description":"캔버스 페이지의 안내선을 지웁니다. axis를 주면 그 축만 지웁니다.","properties":{"page_index":{"type":"integer","minimum":0},"axis":{"type":"string","enum":["columns","rows"]}}}"#;

/// `SetTheme` of a slides deck (task 49a): the arguments are its `ThemePatch`. The engine checks the slot names and the faces.
const SLIDES_THEME: &str = r##"{"type":"object","description":"슬라이드 덱의 테마 색과 글꼴을 바꿉니다. 준 값만 바꾸고 나머지는 그대로 둡니다. 글꼴 이름은 64자 이하이고 제어 문자, <, &를 쓸 수 없습니다.","properties":{"colors":{"type":"object","properties":{"dk1":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"lt1":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"dk2":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"lt2":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"accent1":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"accent2":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"accent3":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"accent4":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"accent5":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"accent6":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"hlink":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"folHlink":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"}}},"major_latin":{"type":"string","minLength":1},"minor_latin":{"type":"string","minLength":1},"major_ea":{"type":"string","minLength":1},"minor_ea":{"type":"string","minLength":1}}}"##;

// The five operations of task 39 (GUI <-> MCP parity): the arguments name what the change set carries.
const PLOT_DISPLAY: &str = r#"{"type":"object","required":["panel_id","on"],"properties":{"panel_id":{"type":"string"},"on":{"type":"boolean"}}}"#;
const IMAGE_REPLACE: &str = r#"{"type":"object","required":["node_id","asset_sha256"],"description":"선택한 캔버스 이미지를 이미 가져온 자산(asset_sha256)으로 바꿉니다. node_id는 지금 선택한 이미지여야 합니다. 파일을 고르려면 이미지 교체 버튼을 쓰세요.","properties":{"node_id":{"type":"string"},"asset_sha256":{"type":"string"}}}"#;
const IMAGE_CROP: &str = r#"{"type":"object","required":["node_id","expected_asset_sha256"],"description":"캔버스 이미지를 원본 픽셀 사각형(x, y, width, height)으로 자릅니다. 네 값을 모두 빼면 자르기를 지웁니다.","properties":{"node_id":{"type":"string"},"expected_asset_sha256":{"type":"string"},"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}}}"#;
const BOARD_MEMOS_SET: &str = r#"{"type":"object","required":["page_id","expected","memos"],"description":"페이지의 보드 메모 목록을 바꿉니다. expected는 지금 목록이고 memos는 바꿀 목록입니다.","properties":{"page_id":{"type":"string"},"expected":{"type":"array"},"memos":{"type":"array"}}}"#;
const PAGE_SECTIONS_SET: &str = r#"{"type":"object","required":["expected","sections"],"description":"캔버스 페이지 섹션 목록을 바꿉니다. expected는 지금 목록이고 sections는 바꿀 목록입니다.","properties":{"expected":{"type":"array"},"sections":{"type":"array"}}}"#;

/// Every command, grouped as in the design: navigation and view, selection, editing, adding,
/// properties, text, AI requests, files and home.
pub const COMMANDS: &[CommandSpec] = &[
    // Screens and navigation.
    CommandSpec { id: "nav.home", label: "홈", permission: Permission::Read, shortcut: "", args: NONE },
    CommandSpec { id: "nav.new", label: "새로 만들기", permission: Permission::Read, shortcut: "Ctrl+N", args: NONE },
    CommandSpec { id: "nav.open_project", label: "작업물 열기", permission: Permission::Read, shortcut: "", args: ARTIFACT },
    CommandSpec { id: "nav.open_file", label: "파일 열기", permission: Permission::Content, shortcut: "Ctrl+O", args: r#"{"type":"object","required":["path"],"description":"열 파일의 절대 경로를 지정합니다.","properties":{"path":{"type":"string"},"kind":{"type":"string","enum":["canvas","web_code","slides","docx","hwpx","hwp","figure"],"description":"작업물 종류. 빼면 확장자나 이 도구의 파일에 적힌 종류로 정합니다."}}}"# },
    CommandSpec { id: "nav.toggle_left", label: "왼쪽 사이드바", permission: Permission::Read, shortcut: "Alt+1", args: NONE },
    CommandSpec { id: "nav.toggle_right", label: "속성 패널", permission: Permission::Read, shortcut: "Ctrl+\\", args: NONE },
    CommandSpec { id: "nav.toggle_panels", label: "패널 모두", permission: Permission::Read, shortcut: "Ctrl+Shift+\\", args: NONE },
    CommandSpec { id: "nav.ai", label: "AI 탭 열기", permission: Permission::Read, shortcut: "Ctrl+J", args: NONE },
    CommandSpec { id: "nav.tab", label: "탭 고르기", permission: Permission::Read, shortcut: "", args: TAB },
    CommandSpec { id: "nav.page", label: "페이지로 가기", permission: Permission::Read, shortcut: "", args: PAGE_INDEX },
    CommandSpec { id: "nav.focus_request", label: "요청 상자에 포커스", permission: Permission::Read, shortcut: "", args: NONE },
    // View.
    CommandSpec { id: "view.zoom_in", label: "확대", permission: Permission::Read, shortcut: "Ctrl+=", args: NONE },
    CommandSpec { id: "view.zoom_out", label: "축소", permission: Permission::Read, shortcut: "Ctrl+-", args: NONE },
    CommandSpec { id: "view.zoom_100", label: "100%", permission: Permission::Read, shortcut: "Ctrl+0", args: NONE },
    CommandSpec { id: "view.zoom_fit", label: "전체 맞춤", permission: Permission::Read, shortcut: "Shift+1", args: NONE },
    CommandSpec { id: "view.zoom_selection", label: "선택 맞춤", permission: Permission::Read, shortcut: "Shift+2", args: NONE },
    CommandSpec { id: "view.zoom_set", label: "배율 지정", permission: Permission::Read, shortcut: "", args: PERCENT },
    CommandSpec { id: "view.play", label: "재생", permission: Permission::Read, shortcut: "F5", args: NONE },
    CommandSpec { id: "view.zoom_page", label: "이 페이지만 보기", permission: Permission::Read, shortcut: "", args: PAGE_INDEX },
    CommandSpec { id: "view.toggle_guides", label: "안내선 보기", permission: Permission::Read, shortcut: "", args: NONE },
    CommandSpec { id: "view.toggle_snap", label: "안내선에 맞춤", permission: Permission::Read, shortcut: "", args: NONE },
    // Selection.
    CommandSpec { id: "select.node", label: "선택", permission: Permission::Read, shortcut: "", args: NODE },
    CommandSpec { id: "select.many", label: "여러 개 선택", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["node_ids"],"properties":{"node_ids":{"type":"array","items":{"type":"string"}}}}"# },
    CommandSpec { id: "select.clear", label: "선택 해제", permission: Permission::Read, shortcut: "Esc", args: NONE },
    CommandSpec { id: "select.next", label: "다음 레이어", permission: Permission::Read, shortcut: "Tab", args: NONE },
    CommandSpec { id: "select.previous", label: "이전 레이어", permission: Permission::Read, shortcut: "Shift+Tab", args: NONE },
    CommandSpec { id: "layer.find", label: "레이어 찾기", permission: Permission::Read, shortcut: "Ctrl+F", args: r#"{"type":"object","properties":{"text":{"type":"string"}}}"# },
    CommandSpec { id: "layer.find.next", label: "다음 찾기", permission: Permission::Read, shortcut: "F3", args: NONE },
    CommandSpec { id: "layer.find.previous", label: "이전 찾기", permission: Permission::Read, shortcut: "Shift+F3", args: NONE },
    // History.
    CommandSpec { id: "edit.undo", label: "실행 취소", permission: Permission::Content, shortcut: "Ctrl+Z", args: NONE },
    CommandSpec { id: "edit.redo", label: "다시 실행", permission: Permission::Content, shortcut: "Ctrl+Y", args: NONE },
    // Node operations.
    CommandSpec { id: "edit.delete", label: "삭제", permission: Permission::Content, shortcut: "Delete", args: NONE },
    CommandSpec { id: "edit.copy", label: "복사", permission: Permission::Read, shortcut: "Ctrl+C", args: NONE },
    CommandSpec { id: "edit.paste", label: "붙여넣기", permission: Permission::Content, shortcut: "Ctrl+V", args: NONE },
    CommandSpec { id: "edit.duplicate", label: "복제", permission: Permission::Content, shortcut: "Ctrl+D", args: NONE },
    CommandSpec { id: "edit.copy_properties", label: "속성 복사", permission: Permission::Read, shortcut: "Ctrl+Alt+C", args: NONE },
    CommandSpec { id: "edit.paste_properties", label: "속성 붙여넣기", permission: Permission::Design, shortcut: "Ctrl+Alt+V", args: NONE },
    CommandSpec { id: "edit.group", label: "그룹", permission: Permission::Content, shortcut: "Ctrl+G", args: NONE },
    CommandSpec { id: "edit.ungroup", label: "그룹 해제", permission: Permission::Content, shortcut: "Ctrl+Shift+G", args: NONE },
    CommandSpec { id: "edit.stack", label: "스택으로 묶기", permission: Permission::Content, shortcut: "Shift+A", args: NONE },
    CommandSpec { id: "edit.order_forward", label: "앞으로", permission: Permission::Content, shortcut: "Ctrl+]", args: ORDER },
    CommandSpec { id: "edit.order_backward", label: "뒤로", permission: Permission::Content, shortcut: "Ctrl+[", args: ORDER },
    CommandSpec { id: "edit.order_front", label: "맨 앞", permission: Permission::Content, shortcut: "Ctrl+Shift+]", args: ORDER },
    CommandSpec { id: "edit.order_back", label: "맨 뒤", permission: Permission::Content, shortcut: "Ctrl+Shift+[", args: ORDER },
    CommandSpec { id: "edit.lock", label: "잠금", permission: Permission::Design, shortcut: "Ctrl+Shift+L", args: NONE },
    CommandSpec { id: "edit.unlock", label: "잠금 해제", permission: Permission::Design, shortcut: "", args: NONE },
    CommandSpec { id: "edit.hide", label: "숨기기", permission: Permission::Content, shortcut: "Ctrl+Shift+H", args: NONE },
    CommandSpec { id: "edit.show", label: "표시", permission: Permission::Content, shortcut: "", args: NONE },
    CommandSpec { id: "edit.nudge", label: "옮기기", permission: Permission::Content, shortcut: "", args: XY },
    CommandSpec { id: "edit.rename", label: "이름 바꾸기", permission: Permission::Content, shortcut: "F2", args: r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"},"node_id":{"type":"string"}}}"# },
    // Adding.
    CommandSpec { id: "add.text", label: "텍스트", permission: Permission::Content, shortcut: "T", args: ARG_AT },
    CommandSpec { id: "add.rect", label: "사각형", permission: Permission::Content, shortcut: "R", args: ARG_AT },
    CommandSpec { id: "add.oval", label: "타원", permission: Permission::Content, shortcut: "O", args: ARG_AT },
    CommandSpec { id: "add.line", label: "선", permission: Permission::Content, shortcut: "L", args: ARG_AT },
    CommandSpec { id: "add.arrow", label: "화살표", permission: Permission::Content, shortcut: "Shift+L", args: NONE },
    CommandSpec { id: "add.frame", label: "프레임", permission: Permission::Content, shortcut: "F", args: ARG_AT },
    CommandSpec { id: "add.pen", label: "펜", permission: Permission::Content, shortcut: "P", args: NONE },
    CommandSpec { id: "add.pencil", label: "연필", permission: Permission::Content, shortcut: "Shift+P", args: NONE },
    CommandSpec { id: "add.memo", label: "메모", permission: Permission::Content, shortcut: "N", args: NONE },
    CommandSpec { id: "add.image", label: "이미지", permission: Permission::Content, shortcut: "Ctrl+Shift+K", args: ARG_AT },
    CommandSpec { id: "add.table", label: "표", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"rows":{"type":"integer","minimum":1},"columns":{"type":"integer","minimum":1}}}"# },
    CommandSpec { id: "canvas.table.add", label: "표", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"rows":{"type":"integer","minimum":1},"columns":{"type":"integer","minimum":1,"maximum":24},"at":{"type":"object","description":"삽입할 위치(add.text와 같은 뜻). 표의 가운데 점을 줍니다.","required":["x","y"],"properties":{"page_index":{"type":"integer","minimum":0},"x":{"type":"number"},"y":{"type":"number"}}}}}"# },
    CommandSpec { id: "canvas.table.add_row", label: "행 추가", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id"],"properties":{"table_id":{"type":"string"},"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "canvas.table.remove_row", label: "행 삭제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id"],"properties":{"table_id":{"type":"string"},"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "canvas.table.add_column", label: "열 추가", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id"],"properties":{"table_id":{"type":"string"},"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "canvas.table.remove_column", label: "열 삭제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id"],"properties":{"table_id":{"type":"string"},"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "canvas.table.set_header", label: "머리글", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id","enabled"],"properties":{"table_id":{"type":"string"},"enabled":{"type":"boolean"}}}"# },
    CommandSpec { id: "canvas.table.set_column_widths", label: "열 너비 비율", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["table_id","column_widths"],"properties":{"table_id":{"type":"string"},"column_widths":{"type":["array","null"],"items":{"type":"integer","minimum":1}}}}"# },
    CommandSpec { id: "table.merge_cells", label: "셀 병합", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id","row","column","rows","columns"],"properties":{"table_id":{"type":"string"},"row":{"type":"integer","minimum":0},"column":{"type":"integer","minimum":0},"rows":{"type":"integer","minimum":1},"columns":{"type":"integer","minimum":1}}}"# },
    CommandSpec { id: "table.unmerge_cell", label: "병합 해제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["table_id","row","column"],"properties":{"table_id":{"type":"string"},"row":{"type":"integer","minimum":0},"column":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "data.edit_cell", label: "칸 값 바꾸기", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","required":["row_id","column_id","value"],"properties":{"dataset_id":{"type":"string"},"row_id":{"type":"string"},"column_id":{"type":"string"},"value":{"type":"string"}}}"# },
    CommandSpec { id: "add.figure_image", label: "이미지", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["path"],"description":"그림 파일의 절대 경로를 지정합니다.","properties":{"path":{"type":"string"},"origin":{"type":"string","enum":["experimental_original","paper_extracted","generated_illustration"]},"source_ref":{"type":"string"}}}"# },
    CommandSpec { id: "data.add_column", label: "열 추가", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","properties":{"dataset_id":{"type":"string"},"name":{"type":"string"},"data_type":{"type":"string","enum":["numeric","category","count","text"]},"unit":{"type":"string"},"fill":{"type":"string"}}}"# },
    CommandSpec { id: "data.add_row", label: "행 추가", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","properties":{"dataset_id":{"type":"string"},"after_row_id":{"type":"string"},"count":{"type":"integer","minimum":1,"maximum":1000}}}"# },
    CommandSpec { id: "data.remove_row", label: "행 삭제", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","required":["row_id"],"properties":{"dataset_id":{"type":"string"},"row_id":{"type":"string"}}}"# },
    CommandSpec { id: "data.remove_column", label: "열 삭제", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","required":["column_id"],"properties":{"dataset_id":{"type":"string"},"column_id":{"type":"string"}}}"# },
    CommandSpec { id: "add.icon", label: "아이콘", permission: Permission::Content, shortcut: "", args: ARG_ICON },
    CommandSpec { id: "add.path", label: "경로", permission: Permission::Content, shortcut: "", args: ARG_PATH },
    CommandSpec { id: "add.paragraph", label: "문단", permission: Permission::Content, shortcut: "", args: NONE },
    CommandSpec { id: "add.page", label: "페이지 추가", permission: Permission::Content, shortcut: "", args: PRESET },
    CommandSpec { id: "add.slide", label: "슬라이드 추가", permission: Permission::Content, shortcut: "Ctrl+M", args: NONE },
    CommandSpec { id: "add.figure_panel", label: "구조도", permission: Permission::Content, shortcut: "", args: NONE },
    CommandSpec { id: "add.diagram_node", label: "상자", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"kind":{"type":"string","enum":["rect","oval"]},"panel_id":{"type":"string"}}}"# },
    CommandSpec { id: "figure.add_equation", label: "수식 넣기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["source"],"properties":{"source":{"type":"string","minLength":1,"description":"수식 원문. 기본 문법: 글자·숫자, x^{2}, x_{i}, \\frac{a}{b}, \\sqrt{x}, \\alpha, \\pm 등"},"panel_id":{"type":"string"}}}"# },
    CommandSpec { id: "add.connector", label: "연결선", permission: Permission::Content, shortcut: "C", args: r#"{"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"},"label":{"type":"string"}}}"# },
    CommandSpec { id: "add.evidence", label: "근거", permission: Permission::Content, shortcut: "", args: NONE },
    CommandSpec { id: "add.csv", label: "CSV 가져오기", permission: Permission::Content, shortcut: "", args: NONE },
    CommandSpec { id: "add.chart", label: "차트", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","properties":{"path":{"type":"string","description":"차트 자료로 읽을 CSV·TSV 파일의 절대 경로. 주면 대화상자 없이 그림에 자료를 넣습니다."}}}"# },
    CommandSpec { id: "add.figure_link", label: "논문 그림", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"figure_id":{"type":"string"}}}"# },
    // Properties (immediate apply; one command = one change set).
    CommandSpec { id: "props.position", label: "위치", permission: Permission::Design, shortcut: "", args: POSITION },
    CommandSpec { id: "props.size", label: "크기", permission: Permission::Design, shortcut: "", args: SIZE },
    CommandSpec { id: "props.size_mode", label: "크기 방식", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"width_mode":{"type":"string","enum":["fixed","hug","fill"]},"height_mode":{"type":"string","enum":["fixed","hug","fill"]}}}"# },
    CommandSpec { id: "props.opacity", label: "불투명도", permission: Permission::Design, shortcut: "", args: PERCENT },
    CommandSpec { id: "props.corner_radius", label: "모서리", permission: Permission::Design, shortcut: "", args: NUMBER_PX },
    CommandSpec { id: "props.image_fit", label: "이미지 맞춤", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["fit"],"properties":{"fit":{"type":"string","enum":["fill","contain","cover"]}}}"# },
    CommandSpec { id: "props.image_alt", label: "대체 텍스트", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["alt"],"properties":{"alt":{"type":"string"}}}"# },
    CommandSpec { id: "props.path", label: "경로 편집", permission: Permission::Content, shortcut: "", args: ARG_NODE_PATH },
    CommandSpec { id: "props.fill", label: "채우기", permission: Permission::Design, shortcut: "", args: COLOR },
    CommandSpec { id: "props.fill_remove", label: "채우기 없애기", permission: Permission::Design, shortcut: "", args: NONE },
    CommandSpec { id: "props.stroke", label: "테두리 색", permission: Permission::Design, shortcut: "", args: COLOR },
    CommandSpec { id: "props.stroke_width", label: "테두리 두께", permission: Permission::Design, shortcut: "", args: NUMBER_PX },
    CommandSpec { id: "props.stroke_remove", label: "테두리 없애기", permission: Permission::Design, shortcut: "", args: NONE },
    CommandSpec { id: "props.shadow", label: "그림자", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"x":{"type":"number"},"y":{"type":"number"},"blur":{"type":"number","minimum":0},"spread":{"type":"number"},"color":{"type":"string"},"opacity":{"type":"number","minimum":0,"maximum":100}}}"# },
    CommandSpec { id: "props.shadow_remove", label: "그림자 없애기", permission: Permission::Design, shortcut: "", args: NONE },
    CommandSpec { id: "props.text_style", label: "글자 모양", permission: Permission::Design, shortcut: "Ctrl+B", args: r#"{"type":"object","properties":{"bold":{"type":"boolean"},"italic":{"type":"boolean"},"font_size":{"type":"number","minimum":1},"color":{"type":"string"}}}"# },
    CommandSpec { id: "canvas.token.put", label: "토큰 저장", permission: Permission::Design, shortcut: "", args: TOKEN_PUT_ARGS },
    CommandSpec { id: "design_theme.apply", label: "디자인 테마 적용", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["entry"],"properties":{"entry":{"type":"object"}}}"# },
    CommandSpec { id: "canvas.token.remove", label: "토큰 삭제", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["token_id"],"description":"사용 중이 아닌 로컬 디자인 토큰을 삭제합니다.","properties":{"token_id":{"type":"string","minLength":1}}}"# },
    CommandSpec { id: "canvas.token.mode", label: "토큰 모드", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["mode"],"description":"캔버스 로컬 토큰을 라이트 또는 다크 모드로 적용합니다.","properties":{"mode":{"type":"string","enum":["light","dark"]}}}"# },
    CommandSpec { id: "canvas.token.bind", label: "선택 항목에 토큰 연결", permission: Permission::Design, shortcut: "", args: TOKEN_BIND_ARGS },
    CommandSpec { id: "canvas.token.unbind", label: "선택 항목에서 토큰 연결 해제", permission: Permission::Design, shortcut: "", args: TOKEN_UNBIND_ARGS },
    CommandSpec { id: "canvas.design_system.list", label: "공유 디자인 시스템 새로 읽기", permission: Permission::Read, shortcut: "", args: NONE },
    CommandSpec { id: "canvas.design_system.publish", label: "디자인 시스템 공개 설정", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["publish"],"properties":{"publish":{"type":"boolean"}}}"# },
    CommandSpec { id: "canvas.design_system.set", label: "공유 디자인 시스템 연결", permission: Permission::Design, shortcut: "", args: DESIGN_SYSTEM_SET_ARGS },
    CommandSpec { id: "props.text_align", label: "문단 정렬", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["align"],"properties":{"align":{"type":"string","enum":["left","center","right","justify"]}}}"# },
    CommandSpec { id: "props.layout_mode", label: "배치", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["mode"],"properties":{"mode":{"type":"string","enum":["free","stack","grid"]}}}"# },
    CommandSpec { id: "props.layout_direction", label: "방향", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["direction"],"properties":{"direction":{"type":"string","enum":["row","column"]}}}"# },
    CommandSpec { id: "props.layout_justify", label: "배분", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["justify"],"properties":{"justify":{"type":"string","enum":["start","center","end","between"]}}}"# },
    CommandSpec { id: "props.layout_align", label: "정렬", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["align"],"properties":{"align":{"type":"string","enum":["start","center","end","stretch"]}}}"# },
    CommandSpec { id: "props.layout_gap", label: "간격", permission: Permission::Design, shortcut: "", args: NUMBER_PX },
    CommandSpec { id: "props.layout_padding", label: "안쪽 여백", permission: Permission::Design, shortcut: "", args: NUMBER_PX },
    CommandSpec { id: "props.align", label: "정렬 6", permission: Permission::Design, shortcut: "Alt+A", args: ALIGN },
    CommandSpec { id: "props.distribute", label: "균등 분배", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["axis"],"properties":{"axis":{"type":"string","enum":["h","v"]}}}"# },
    CommandSpec { id: "props.page_size", label: "페이지 크기", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"preset":{"type":"string"},"width":{"type":"number"},"height":{"type":"number"}}}"# },
    CommandSpec { id: "props.rename_page", label: "페이지 이름", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"# },
    // Effects and grid fields (GUI_DESIGN §9.1; lengths in px on canvases and slides, pt elsewhere).
    CommandSpec { id: "props.line_height", label: "줄 높이", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"value":{"type":"number","minimum":0}}}"# },
    CommandSpec { id: "props.letter_spacing", label: "자간", permission: Permission::Design, shortcut: "", args: NUMBER_PX },
    CommandSpec { id: "props.vertical_align", label: "세로 정렬", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["align"],"properties":{"align":{"type":"string","enum":["top","middle","bottom"]}}}"# },
    CommandSpec { id: "props.stroke_style", label: "선 모양", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"dash":{"type":"string","enum":["solid","dashed","dotted"]},"cap":{"type":"string","enum":["butt","round","square"]},"join":{"type":"string","enum":["miter","round","bevel"]}}}"# },
    CommandSpec { id: "props.gradient", label: "그라디언트", permission: Permission::Design, shortcut: "", args: r##"{"type":"object","required":["from","to"],"properties":{"from":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"to":{"type":"string","pattern":"^#[0-9A-Fa-f]{6}$"},"angle":{"type":"number"}}}"## },
    CommandSpec { id: "props.gradient_remove", label: "단색으로", permission: Permission::Design, shortcut: "", args: NONE },
    CommandSpec { id: "props.grid", label: "그리드", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"columns":{"type":"integer","minimum":1},"row_gap":{"type":"number","minimum":0}}}"# },
    CommandSpec { id: "props.grid_span", label: "칸", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["span"],"properties":{"span":{"type":"integer","minimum":1},"node_id":{"type":"string"}}}"# },
    CommandSpec { id: "props.padding_sides", label: "네 면 여백", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"top":{"type":"number","minimum":0},"right":{"type":"number","minimum":0},"bottom":{"type":"number","minimum":0},"left":{"type":"number","minimum":0}}}"# },
    CommandSpec { id: "props.align_self", label: "자체 정렬", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["align"],"properties":{"align":{"type":"string","enum":["auto","start","center","end","stretch"]},"node_id":{"type":"string"},"node_ids":{"type":"array","items":{"type":"string"}}}}"# },
    CommandSpec { id: "props.variant_style", label: "상태 스타일", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["variant"],"properties":{"variant":{"type":"string"},"node_id":{"type":"string"},"fill":{"type":"string"},"stroke":{"type":"string"},"color":{"type":"string"},"opacity":{"type":"number","minimum":0,"maximum":100},"bold":{"type":"boolean"},"italic":{"type":"boolean"},"font_size":{"type":"number","minimum":1}}}"# },
    CommandSpec { id: "canvas.open_url", label: "URL 링크", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["url"],"properties":{"url":{"type":"string","maxLength":2048}}}"# },
    CommandSpec { id: "canvas.interaction.set", label: "클릭 동작 변경", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["index","action"],"properties":{"index":{"type":"integer","minimum":0},"action":{"type":"string","enum":["navigate_page","toggle_visibility","set_variant","open_url","back"]},"target_id":{"type":"string"},"variant":{"type":"string"},"url":{"type":"string","maxLength":2048}}}"# },
    CommandSpec { id: "canvas.interaction.remove", label: "클릭 동작 삭제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["index"],"properties":{"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "canvas.image.replace", label: "이미지 교체", permission: Permission::Content, shortcut: "", args: IMAGE_REPLACE },
    CommandSpec { id: "canvas.image.crop", label: "이미지 자르기", permission: Permission::Design, shortcut: "", args: IMAGE_CROP },
    CommandSpec { id: "canvas.board_memos.set", label: "보드 메모 저장", permission: Permission::Content, shortcut: "", args: BOARD_MEMOS_SET },
    CommandSpec { id: "canvas.page_sections.set", label: "페이지 섹션 저장", permission: Permission::Content, shortcut: "", args: PAGE_SECTIONS_SET },
    CommandSpec { id: "layer.move_to", label: "레이어를 다른 곳으로", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["node_id","index"],"properties":{"node_id":{"type":"string"},"parent_id":{"type":"string"},"page_index":{"type":"integer","minimum":0},"index":{"type":"integer","minimum":0}}}"# },
    // Fixes a refusal callout offers for a lint finding (canvases and slides; lengths in px).
    CommandSpec { id: "fix.grow_to_min", label: "44 px로 키우기", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"},"min_w":{"type":"number","minimum":1},"min_h":{"type":"number","minimum":1}}}"# },
    CommandSpec { id: "fix.darken_text", label: "글자색 진하게", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"},"min_ratio":{"type":"number","minimum":1}}}"# },
    CommandSpec { id: "fix.grow_box", label: "상자 키우기", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"}}}"# },
    CommandSpec { id: "fix.shrink_text", label: "글자 줄이기", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"}}}"# },
    CommandSpec { id: "fix.grow_parent", label: "상위 프레임 키우기", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"}}}"# },
    CommandSpec { id: "fix.move_into_page", label: "페이지 안으로", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"}}}"# },
    // Pages of canvases and slides.
    CommandSpec { id: "page.remove", label: "페이지 삭제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"page_index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "page.reorder", label: "페이지 순서", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["to_index"],"properties":{"page_index":{"type":"integer","minimum":0},"to_index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "page.notes", label: "노트", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["notes"],"properties":{"notes":{"type":"string"},"page_index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "slides.theme", label: "테마", permission: Permission::Design, shortcut: "", args: SLIDES_THEME },
    CommandSpec { id: "canvas.guides.set", label: "안내선 설정", permission: Permission::Design, shortcut: "", args: GUIDES_SET },
    CommandSpec { id: "canvas.guides.clear", label: "안내선 지우기", permission: Permission::Design, shortcut: "", args: GUIDES_CLEAR },
    CommandSpec { id: "page.component", label: "컴포넌트 소스 페이지", permission: Permission::Content, shortcut: "", args: PAGE_COMPONENT },
    CommandSpec { id: "component.instantiate", label: "컴포넌트 삽입", permission: Permission::Content, shortcut: "", args: COMPONENT_INSTANTIATE },
    CommandSpec { id: "component.detach", label: "컴포넌트 연결 해제", permission: Permission::Content, shortcut: "", args: NODE },
    // Repeat and conditional groups (D21). The arguments are the ids and the values of one step; the stored spec is read from the document.
    CommandSpec { id: "repeat.create", label: "반복 만들기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["node_id","table_id"],"description":"최상위 항목을 컴포넌트 페이지로 옮기고, 같은 자리에 표의 데이터 행마다 그 컴포넌트를 펼칠 빈 그룹을 만듭니다.","properties":{"node_id":{"type":"string"},"table_id":{"type":"string"},"component_page_id":{"type":"string"},"group_id":{"type":"string"}}}"# },
    CommandSpec { id: "repeat.bind", label: "반복 열 연결", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["group_id","bindings"],"properties":{"group_id":{"type":"string"},"bindings":{"type":"array","items":{"type":"object","required":["source_node_id","column"],"properties":{"source_node_id":{"type":"string"},"column":{"type":"integer","minimum":0}}}}}}"# },
    CommandSpec { id: "repeat.clear", label: "반복 해제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["group_id"],"properties":{"group_id":{"type":"string"}}}"# },
    CommandSpec { id: "repeat.condition_set", label: "조건 설정", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["node_id","column","op"],"properties":{"node_id":{"type":"string"},"column":{"type":"integer","minimum":0},"op":{"type":"string","enum":["equals","not_equals","is_empty","is_not_empty"]},"value":{"type":"string"}}}"# },
    CommandSpec { id: "repeat.condition_clear", label: "조건 해제", permission: Permission::Content, shortcut: "", args: NODE },
    // Paper figures.
    CommandSpec { id: "figure.arrange_panels", label: "패널 다시 배치", permission: Permission::Design, shortcut: "", args: NONE },
    CommandSpec { id: "figure.remove_panel", label: "패널 삭제", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"panel_id":{"type":"string"}}}"# },
    CommandSpec { id: "figure.reorder_panels", label: "패널 순서", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["index"],"properties":{"panel_id":{"type":"string"},"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "figure.caption", label: "캡션", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["caption"],"properties":{"caption":{"type":"string"}}}"# },
    CommandSpec { id: "figure.panel_label", label: "패널 라벨", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["label"],"properties":{"label":{"type":"string"},"panel_id":{"type":"string"}}}"# },
    CommandSpec { id: "figure.plot", label: "차트 설정", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","properties":{"panel_id":{"type":"string"},"kind":{"type":"string","enum":["bar","line","scatter"]},"x_column":{"type":"string"},"y_columns":{"type":"array","items":{"type":"string"}},"x_minimum":{"type":["number","null"]},"x_maximum":{"type":["number","null"]},"y_minimum":{"type":["number","null"]},"y_maximum":{"type":["number","null"]},"x_scale":{"type":"string","enum":["linear","log10"]},"y_scale":{"type":"string","enum":["linear","log10"]},"x_unit":{"type":"string"},"y_unit":{"type":"string"},"error_kind":{"type":"string","enum":["none","sd","sem","ci","range"]},"error_column":{"type":"string"},"error_lower_column":{"type":"string"},"error_upper_column":{"type":"string"},"error_count_column":{"type":"string"},"confidence_level":{"type":"string"}}}"# },
    CommandSpec { id: "figure.plot_display", label: "오차 기준 표시", permission: Permission::Design, shortcut: "", args: PLOT_DISPLAY },
    CommandSpec { id: "figure.style", label: "요소 모양", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"target_id":{"type":"string"},"fill":{"type":"string"},"stroke":{"type":"string"},"stroke_width":{"type":"number","minimum":0},"color":{"type":"string"},"font_size":{"type":"number","minimum":1},"bold":{"type":"boolean"},"italic":{"type":"boolean"},"opacity":{"type":"number","minimum":0,"maximum":100},"dash":{"type":"string","enum":["solid","dashed","dotted"]}}}"# },
    CommandSpec { id: "figure.toggle_sheet", label: "데이터 시트", permission: Permission::Read, shortcut: "", args: NONE },
    CommandSpec { id: "figure.axis_label", label: "축 제목", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["axis","label"],"properties":{"axis":{"type":"string","enum":["x","y"]},"label":{"type":"string"},"panel_id":{"type":"string"}}}"# },
    CommandSpec { id: "figure.axis_ticks", label: "눈금 간격", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["axis","tick_step"],"properties":{"axis":{"type":"string","enum":["x","y"]},"tick_step":{"type":["number","null"],"minimum":0,"description":"눈금 간격. null이면 자동 눈금으로 되돌립니다."},"panel_id":{"type":"string"}}}"# },
    CommandSpec { id: "figure.put_dataset", label: "자료 넣기", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","required":["dataset"],"properties":{"dataset":{"type":"object"}}}"# },
    CommandSpec { id: "figure.put_evidence", label: "근거 넣기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"evidence":{"type":"object"},"text":{"type":"string"}}}"# },
    CommandSpec { id: "figure.diagram_provenance", label: "출처와 근거", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["element_id","origin","evidence_ids"],"properties":{"panel_id":{"type":"string"},"element_id":{"type":"string","minLength":1},"origin":{"type":["string","null"],"enum":["paper_evidence","user_authored","model_proposal"],"description":"도형의 출처. null이면 출처를 지웁니다. 연결선은 출처를 비울 수 없습니다."},"evidence_ids":{"type":"array","items":{"type":"string","minLength":1},"description":"그림에 있는 근거의 id 목록. 빈 배열이면 근거 연결을 지웁니다."}}}"# },
    CommandSpec { id: "figure.common_legend", label: "공통 범례", permission: Permission::Content, shortcut: "", args: r##"{"type":"object","required":["legend"],"properties":{"legend":{"description":"공통 범례 전체. null이면 범례를 지웁니다.","oneOf":[{"type":"object","required":["items"],"properties":{"items":{"type":"array","items":{"type":"object","required":["label","color"],"properties":{"label":{"type":"string","minLength":1},"color":{"type":"string","description":"#RRGGBB 또는 #RRGGBBAA"}}}},"placement":{"type":"string","enum":["bottom","top","right"]},"title":{"type":"string"}}},{"type":"null"}]}}}"## },
    CommandSpec { id: "figure.output_profile", label: "출력 프로필 적용", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["profile"],"properties":{"profile":{"type":"string","enum":["single_column","one_half_column","double_column","high_res_print","presentation_16_9"]}}}"# },
    CommandSpec { id: "figure.visual_review", label: "시각 검토", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["review"],"properties":{"review":{"description":"시각 검토 전체. null이면 검토를 지웁니다. 승인하면 엔진이 지금 내용을 기록합니다.","oneOf":[{"type":"object","required":["state"],"properties":{"state":{"type":"string","enum":["pending","approved","needs_revision"]},"note":{"type":"string"}}},{"type":"null"}]}}}"# },
    CommandSpec { id: "figure.image_transform", label: "그림 자르기·회전", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["crop","rotation_millidegrees"],"properties":{"panel_id":{"type":"string"},"crop":{"description":"자를 영역(그림 픽셀). null이면 자르기를 해제합니다.","oneOf":[{"type":"object","required":["x","y","width","height"],"properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}}},{"type":"null"}]},"rotation_millidegrees":{"type":"integer","minimum":-359999,"maximum":359999,"description":"회전 각도(1/1000도). 360도 이상은 쓸 수 없습니다."}}}"# },
    CommandSpec { id: "figure.image_calibration", label: "측정 보정", permission: Permission::Data, shortcut: "", args: r#"{"type":"object","required":["calibration"],"properties":{"panel_id":{"type":"string"},"calibration":{"description":"측정 보정. null이면 보정을 지웁니다.","oneOf":[{"type":"object","required":["length","unit","pixel_span"],"properties":{"length":{"type":"string","minLength":1,"description":"실제 길이. 0보다 큰 십진수 문자열"},"unit":{"type":"string","minLength":1},"pixel_span":{"type":"string","minLength":1,"description":"같은 길이의 픽셀 간격. 0보다 큰 십진수 문자열"},"evidence_id":{"type":"string","minLength":1}}},{"type":"null"}]}}}"# },
    // Text documents.
    CommandSpec { id: "doc.paragraph_format", label: "문단 서식", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"paragraph_id":{"type":"string"},"align":{"type":"string","enum":["left","center","right","justify"]},"indent":{"type":"number"},"space_before":{"type":"number","minimum":0},"space_after":{"type":"number","minimum":0},"line_spacing":{"type":"number","minimum":0}}}"# },
    CommandSpec { id: "doc.page_setup", label: "쪽 설정", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"section_id":{"type":"string"},"paper":{"type":"string","enum":["a4","letter"]},"orientation":{"type":"string","enum":["portrait","landscape"]},"width":{"type":"number","minimum":1},"height":{"type":"number","minimum":1},"margin_left":{"type":"number","minimum":0},"margin_top":{"type":"number","minimum":0},"margin_right":{"type":"number","minimum":0},"margin_bottom":{"type":"number","minimum":0}}}"# },
    CommandSpec { id: "doc.named_style", label: "스타일 바꾸기", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"},"font_family":{"type":"string"},"font_size":{"type":"number","minimum":1},"color":{"type":"string"},"bold":{"type":"boolean"},"italic":{"type":"boolean"},"clear_overrides":{"type":"boolean"}}}"# },
    CommandSpec { id: "doc.table_columns", label: "열 너비", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","required":["column_widths"],"properties":{"table_id":{"type":"string"},"column_widths":{"type":"array","items":{"type":"number"}}}}"# },
    // Text.
    CommandSpec { id: "text.set", label: "글 바꾸기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["node_id","text"],"properties":{"node_id":{"type":"string"},"text":{"type":"string"}}}"# },
    CommandSpec { id: "text.replace", label: "문단 글 바꾸기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["text"],"properties":{"text":{"type":"string"},"paragraph_id":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "text.style_range", label: "고른 글자 모양", permission: Permission::Design, shortcut: "", args: r#"{"type":"object","properties":{"paragraph_id":{"type":"string"},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},"bold":{"type":"boolean"},"italic":{"type":"boolean"},"underline":{"type":"boolean"},"strikethrough":{"type":"boolean"},"font_size":{"type":"number","minimum":1},"font_family":{"type":"string"},"color":{"type":"string"}}}"# },
    CommandSpec { id: "text.list", label: "목록", permission: Permission::Content, shortcut: "", args: TEXT_LIST },
    CommandSpec { id: "web.replace_source", label: "소스 바꾸기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["content"],"properties":{"path":{"type":"string"},"content":{"type":"string"},"expected_hash":{"type":"string"}}}"# },
    CommandSpec { id: "web.mapped_edit", label: "매핑 값 바꾸기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["mapping_id","property","value"],"properties":{"path":{"type":"string"},"mapping_id":{"type":"string","minLength":1},"property":{"type":"string","minLength":1},"value":{"type":"string"},"expected_hash":{"type":"string"}}}"# },
    CommandSpec { id: "web.open_file", label: "파일 열기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string","minLength":1}}}"# },
    // AI requests.
    CommandSpec { id: "ai.request", label: "요청 보내기", permission: Permission::Content, shortcut: "Ctrl+Enter", args: PROMPT },
    CommandSpec { id: "ai.cancel", label: "취소", permission: Permission::Content, shortcut: "", args: AI_CANCEL },
    CommandSpec { id: "ai.retry", label: "다시 요청", permission: Permission::Content, shortcut: "", args: NONE },
    CommandSpec { id: "ai.undo_change", label: "되돌리기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["change_id"],"properties":{"change_id":{"type":"string"}}}"# },
    CommandSpec { id: "layer.move", label: "레이어 옮기기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["node_id","index"],"properties":{"node_id":{"type":"string"},"index":{"type":"integer","minimum":0}}}"# },
    CommandSpec { id: "ai.toggle_intent", label: "의도 고르기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","anyOf":[{"type":"object","required":["intent"]},{"type":"object","required":["mode"]}],"properties":{"intent":{"type":"string","enum":["content","design","data"]},"mode":{"type":"string","enum":["both","design_only","content_only"]}}}"# },
    CommandSpec { id: "ai.deliver_as", label: "미리보기로 받기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["deliver_as"],"properties":{"deliver_as":{"type":"string","enum":["apply","proposal"]}}}"# },
    CommandSpec { id: "proposal.select", label: "미리보기 고르기", permission: Permission::Read, shortcut: "", args: PROPOSAL_ID },
    CommandSpec { id: "proposal.apply", label: "미리보기 적용", permission: Permission::Content, shortcut: "", args: PROPOSAL_ID },
    CommandSpec { id: "proposal.discard", label: "미리보기 버리기", permission: Permission::Content, shortcut: "", args: PROPOSAL_ID },
    // Files and export.
    CommandSpec { id: "file.save", label: "저장", permission: Permission::FileWrite, shortcut: "Ctrl+S", args: FILE_SAVE },
    CommandSpec { id: "file.save_as", label: "다른 이름으로 저장", permission: Permission::FileWrite, shortcut: "Ctrl+Shift+S", args: NONE },
    CommandSpec { id: "file.export", label: "내보내기", permission: Permission::FileWrite, shortcut: "Ctrl+E", args: FILE_EXPORT },
    CommandSpec { id: "file.export_selection", label: "선택 내보내기", permission: Permission::FileWrite, shortcut: "", args: r#"{"type":"object","description":"선택한 캔버스 항목만 내보냅니다.","required":["format","target_path"],"properties":{"format":{"type":"string","enum":["png","svg"]},"target_path":{"type":"string"},"overwrite":{"type":"boolean"},"expected_target_hash":{"type":"string"},"allowed_losses":{"type":"array","items":{"type":"string"}},"scale":{"type":"integer","enum":[1,2,3]},"background":{"type":"string","enum":["page","transparent"]}}}"# },
    CommandSpec { id: "file.overwrite", label: "원본에 덮어쓰기", permission: Permission::FileWrite, shortcut: "", args: NONE },
    CommandSpec { id: "file.dismiss_loss", label: "손실 허용하고 내보내기", permission: Permission::FileWrite, shortcut: "", args: NONE },
    // Figures and web.
    CommandSpec { id: "figure.update_link", label: "그림 갱신", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"}}}"# },
    CommandSpec { id: "figure.open_source", label: "원본 열기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","properties":{"node_id":{"type":"string"}}}"# },
    CommandSpec { id: "web.refresh", label: "새로 고침", permission: Permission::Read, shortcut: "", args: NONE },
    CommandSpec { id: "web.preview", label: "웹 미리보기 열기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","description":"현재 웹 작업물을 미리보기로 열고 페이지가 준비되면 작업물 ID와 판 번호를 반환합니다."}"# },
    // Home.
    CommandSpec { id: "home.create", label: "만들기", permission: Permission::Content, shortcut: "Enter", args: CREATE },
    CommandSpec { id: "home.attach", label: "첨부", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}"# },
    CommandSpec { id: "home.view", label: "목록 또는 격자", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["view"],"properties":{"view":{"type":"string","enum":["list","grid"]}}}"# },
    CommandSpec { id: "home.starred_only", label: "별표만", permission: Permission::Read, shortcut: "", args: BOOL },
    CommandSpec { id: "home.star", label: "별표", permission: Permission::Read, shortcut: "", args: ARTIFACT },
    CommandSpec { id: "home.mode", label: "디자인 또는 코드", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["mode"],"properties":{"mode":{"type":"string","enum":["design","code"]}}}"# },
    CommandSpec { id: "home.design_system", label: "디자인 시스템 고르기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","properties":{"design_system":{"type":"string","enum":[""]}}}"# },
    CommandSpec { id: "home.tile", label: "타일 고르기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["tile"],"properties":{"tile":{"type":"string","enum":["blank","mobile","ui","wireframe","slides","document","resume","poster","social","figure","diagram","web","animation"]}}}"# },
    CommandSpec { id: "home.tab", label: "홈 탭", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["tab"],"properties":{"tab":{"type":"string","enum":["projects","design_systems","templates"]}}}"# },
    CommandSpec { id: "home.preset", label: "크기 고르기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","properties":{"preset":{"type":"string"},"width":{"type":"number","minimum":1},"height":{"type":"number","minimum":1}}}"# },
    CommandSpec { id: "home.runner", label: "실행기 고르기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","properties":{"runtime":{"type":"string","enum":["claude","agy","codex"]},"model":{"type":"string"},"effort":{"type":"string","enum":["minimal","low","medium","high","xhigh","max"]}}}"# },
    CommandSpec { id: "home.search", label: "프로젝트 찾기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","required":["text"],"properties":{"text":{"type":"string"}}}"# },
    CommandSpec { id: "home.use_template", label: "템플릿으로 만들기", permission: Permission::Content, shortcut: "", args: r#"{"type":"object","required":["template"],"properties":{"template":{"type":"string","enum":["mobile_app","login","landing_page","dashboard","social_post","poster_a3","resume","invitation","banner","webgl_animation"]},"title":{"type":"string"},"kind":{"type":"string","enum":["canvas","web_code"]}}}"# },
    CommandSpec { id: "home.workspace", label: "작업공간 열기", permission: Permission::Read, shortcut: "", args: r#"{"type":"object","properties":{"path":{"type":"string"}}}"# },
];

/// Commands whose engine path or control is not built yet. They stay in the registry, agents see
/// them as `enabled: false`, and no control on screen shows them (no control that does not work).
pub const NOT_WIRED: &[&str] = &[
    "web.refresh",
    "add.pen", "add.pencil", "add.memo", "add.arrow",
    // A window serves one workspace host; opening another workspace needs the host to restart on it.
    "file.overwrite", "home.workspace",
];

/// Whether the command has a built engine path and control.
pub fn available(id: &str) -> bool {
    !NOT_WIRED.contains(&id)
}

/// The permission class the UI channel reports: starting an AI job is `agent_start`, design and
/// content changes are `change`, a file write is `file_write`, and the rest is `read`.
pub fn ui_permission(spec: &CommandSpec) -> UiPermission {
    match (spec.id, spec.permission) {
        ("ai.request" | "ai.retry" | "home.create", _) => UiPermission::AgentStart,
        (_, Permission::Read) => UiPermission::Read,
        (_, Permission::Design | Permission::Content | Permission::Data) => UiPermission::Change,
        (_, Permission::FileWrite) => UiPermission::FileWrite,
    }
}

/// The intents a shared design-system link changes in a document of `kind`. A WebCode project's link also writes the entry file's
/// stylesheet link, so it changes content too. The UI listing, the run check and the GUI dispatch all read this one definition.
pub fn design_system_link_intents(kind: ArtifactKind) -> BTreeSet<Intent> {
    match kind {
        ArtifactKind::WebCode => [Intent::Design, Intent::Content].into_iter().collect(),
        _ => [Intent::Design].into_iter().collect(),
    }
}

/// The intents a command changes in a document of `kind`; a run must allow every one of them. Starting a job changes nothing by
/// itself.
pub fn ui_intents_for(spec: &CommandSpec, kind: ArtifactKind) -> BTreeSet<Intent> {
    // The AI intent switch and a proposal's apply (its change set may name any intent) name all of them.
    if spec.id == "ai.toggle_intent" || spec.id == "proposal.apply" {
        return [Intent::Design, Intent::Content, Intent::Data].into_iter().collect();
    }
    if spec.id == "canvas.design_system.set" {
        return design_system_link_intents(kind);
    }
    if spec.id == "add.figure_image" {
        return [Intent::Content, Intent::Design].into_iter().collect();
    }
    match (ui_permission(spec), spec.permission) {
        (UiPermission::Change, Permission::Design) => [Intent::Design].into_iter().collect(),
        (UiPermission::Change, Permission::Data) => [Intent::Data].into_iter().collect(),
        (UiPermission::Change, _) => [Intent::Content].into_iter().collect(),
        _ => BTreeSet::new(),
    }
}

/// `ui_intents_for` a Canvas document: the intents the registry snapshot describes.
#[cfg(test)]
pub fn ui_intents(spec: &CommandSpec) -> BTreeSet<Intent> {
    ui_intents_for(spec, ArtifactKind::Canvas)
}

/// The command with this id.
pub fn find(id: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.id == id)
}

/// Checks `args` against the command's schema. Supports the subset the table uses: an object with
/// `required`, typed properties (string, number, integer, boolean), `enum`, `minimum`, `maximum`
/// and `pattern` for strings. Extra keys are allowed. A null for an optional property is the same as
/// an absent one, unless the property's own schema lists null (see `absent_nulls`).
pub fn validate(spec: &CommandSpec, args: &Value) -> Result<(), String> {
    let schema: Value = serde_json::from_str(spec.args).map_err(|error| format!("schema of {}: {error}", spec.id))?;
    check(&schema, args, spec.id, &[])
}

/// The arguments with the nulls that `validate` treats as absent removed, down through the declared object properties,
/// their `oneOf`/`anyOf` alternatives and the array items. A handler then sees such a property absent, as `validate`
/// already does. A null for a required property stays, and `validate` refuses it unless the property's schema lists null.
pub fn without_absent_nulls(spec: &CommandSpec, mut args: Value) -> Value {
    if let Ok(schema) = serde_json::from_str::<Value>(spec.args) {
        remove_absent_nulls(&schema, &mut args);
    }
    args
}

/// `inherited` names the keys that are absent from the object `value` at the level above: alternatives are
/// checked against that same object, so they must see the same absent keys.
fn check(schema: &Value, value: &Value, path: &str, inherited: &[&str]) -> Result<(), String> {
    let mut absent: Vec<&str> = inherited.to_vec();
    if let Some(map) = value.as_object() {
        absent.extend(absent_nulls(schema, map));
    }
    if let Some(alternatives) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = alternatives.iter().filter(|alternative| check(alternative, value, path, &absent).is_ok()).count();
        return if matches == 1 { Ok(()) } else { Err(format!("{path}: expected exactly one allowed value shape")) };
    }
    if let Some(alternatives) = schema.get("anyOf").and_then(Value::as_array) {
        if !alternatives.iter().any(|alternative| check(alternative, value, path, &absent).is_ok()) {
            return Err(format!("{path}: none of the allowed argument sets is given"));
        }
    }
    // A type is one name or a list of names (`["array", "null"]`); a schema without one accepts any value.
    let kinds: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    if !kinds.iter().any(|kind| type_matches(kind, value)) {
        return Err(format!("{path}: expected {}", kinds.join(" or ")));
    }
    if value.is_null() {
        return Ok(());
    }
    if let Some(map) = value.as_object() {
        for key in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !map.contains_key(key) || absent.contains(&key) {
                return Err(format!("{path}: missing `{key}`"));
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, property) in properties {
                if absent.contains(&key.as_str()) {
                    continue;
                }
                if let Some(child) = map.get(key) {
                    check(property, child, &format!("{path}.{key}"), &[])?;
                }
            }
        }
        return Ok(());
    }
    if let Some(items) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in items.iter().enumerate() {
                check(item_schema, item, &format!("{path}[{index}]"), &[])?;
            }
        }
        return Ok(());
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            return Err(format!("{path}: `{value}` is not one of the allowed values"));
        }
    }
    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
            if number < minimum {
                return Err(format!("{path}: must be at least {minimum}"));
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
            if number > maximum {
                return Err(format!("{path}: must be at most {maximum}"));
            }
        }
    }
    if let (Some(text), Some(pattern)) = (value.as_str(), schema.get("pattern").and_then(Value::as_str)) {
        if !matches_hex_colour(pattern, text) {
            return Err(format!("{path}: does not match {pattern}"));
        }
    }
    if let (Some(text), Some(minimum)) = (value.as_str(), schema.get("minLength").and_then(Value::as_u64)) {
        if text.chars().count() < minimum as usize {
            return Err(format!("{path}: must contain at least {minimum} characters"));
        }
    }
    Ok(())
}

/// The keys of `map` that count as absent under `schema`: a null for a declared property that the schema does not
/// require and that refuses null. A property whose schema lists null keeps its null as a value of its own: a
/// figure's axis limit is cleared by null, and absence would keep it.
fn absent_nulls<'a>(schema: &Value, map: &'a serde_json::Map<String, Value>) -> Vec<&'a str> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .filter(|(key, value)| {
            value.is_null()
                && !is_required(schema, key.as_str())
                && properties.get(key.as_str()).is_some_and(|property| check(property, value, "", &[]).is_err())
        })
        .map(|(key, _)| key.as_str())
        .collect()
}

fn is_required(schema: &Value, key: &str) -> bool {
    schema.get("required").and_then(Value::as_array).is_some_and(|names| names.iter().any(|name| name.as_str() == Some(key)))
}

/// The schema and each `oneOf`/`anyOf` alternative below it: `check` applies every one of them to the same value.
fn schema_levels<'a>(schema: &'a Value, levels: &mut Vec<&'a Value>) {
    levels.push(schema);
    for keyword in ["oneOf", "anyOf"] {
        for alternative in schema.get(keyword).and_then(Value::as_array).into_iter().flatten() {
            schema_levels(alternative, levels);
        }
    }
}

/// Whether a level declares `key` with a type that takes null: then its null is a value, not an absence.
fn declares_null(levels: &[&Value], key: &str) -> bool {
    levels.iter().any(|level| {
        level
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(key))
            .is_some_and(|property| check(property, &Value::Null, "", &[]).is_ok())
    })
}

/// Removes the nulls that `check` reads as absent, at every level it checks: the value's schema and its `oneOf`/`anyOf`
/// alternatives, down through the declared object properties and the array items. A key that the value's own schema
/// requires keeps its null, and so does a key that some level declares as taking null.
fn remove_absent_nulls(schema: &Value, value: &mut Value) {
    let mut levels = Vec::new();
    schema_levels(schema, &mut levels);
    if let Some(items) = value.as_array_mut() {
        for item_schema in levels.iter().filter_map(|&level| level.get("items")) {
            for item in items.iter_mut() {
                remove_absent_nulls(item_schema, item);
            }
        }
        return;
    }
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let mut absent: Vec<String> = Vec::new();
    for &level in &levels {
        for key in absent_nulls(level, map) {
            if !is_required(schema, key) && !declares_null(&levels, key) && !absent.iter().any(|known| known.as_str() == key) {
                absent.push(key.to_owned());
            }
        }
    }
    for key in &absent {
        map.remove(key);
    }
    for &level in &levels {
        for (key, property) in level.get("properties").and_then(Value::as_object).into_iter().flatten() {
            if let Some(child) = map.get_mut(key) {
                remove_absent_nulls(property, child);
            }
        }
    }
}

/// Whether a value has the JSON type a schema names. A name the checker does not know accepts any value.
fn type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_u64() || value.is_i64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// The only pattern the table uses: a `#RRGGBB` colour. Other patterns are accepted unchecked.
fn matches_hex_colour(pattern: &str, text: &str) -> bool {
    if pattern != "^#[0-9A-Fa-f]{6}$" {
        return true;
    }
    text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// The keyboard shortcut for a command as egui sees it, when the table names one. Parsing covers
/// the names the table uses: Ctrl/Shift/Alt, letters, digits, and the keys listed below.
pub fn keyboard_shortcut(spec: &CommandSpec) -> Option<KeyboardShortcut> {
    if spec.shortcut.is_empty() {
        return None;
    }
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    for part in spec.shortcut.split('+') {
        match part {
            "Ctrl" => modifiers = modifiers | Modifiers::COMMAND,
            "Shift" => modifiers = modifiers | Modifiers::SHIFT,
            "Alt" => modifiers = modifiers | Modifiers::ALT,
            other => key = key_named(other),
        }
    }
    key.map(|key| KeyboardShortcut::new(modifiers, key))
}

fn key_named(name: &str) -> Option<Key> {
    Some(match name {
        "Delete" | "Del" => Key::Delete,
        "Enter" => Key::Enter,
        "Esc" => Key::Escape,
        "Tab" => Key::Tab,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F5" => Key::F5,
        "\\" => Key::Backslash,
        "]" => Key::CloseBracket,
        "[" => Key::OpenBracket,
        "=" => Key::Equals,
        "-" => Key::Minus,
        "0" => Key::Num0,
        "1" => Key::Num1,
        "2" => Key::Num2,
        "A" => Key::A,
        "B" => Key::B,
        "C" => Key::C,
        "D" => Key::D,
        "E" => Key::E,
        "F" => Key::F,
        "G" => Key::G,
        "H" => Key::H,
        "I" => Key::I,
        "J" => Key::J,
        "K" => Key::K,
        "L" => Key::L,
        "M" => Key::M,
        "N" => Key::N,
        "O" => Key::O,
        "P" => Key::P,
        "R" => Key::R,
        "S" => Key::S,
        "T" => Key::T,
        "V" => Key::V,
        "Y" => Key::Y,
        "Z" => Key::Z,
        _ => return None,
    })
}

/// True for commands whose shortcut is a bare letter or symbol. Those must not fire while a text
/// field has the keyboard (typing `T` in a label must not select the text tool).
pub fn is_bare_key(spec: &CommandSpec) -> bool {
    let is_function_key = spec.shortcut.len() > 1 && spec.shortcut.starts_with('F') && spec.shortcut[1..].chars().all(|c| c.is_ascii_digit());
    !spec.shortcut.is_empty() && !spec.shortcut.contains('+') && !is_function_key
}

/// The core calls a command's arm sends, as the arm states them: `method`, or `method:operation` for a change set.
/// The UI channel uses this declaration to expose only commands whose document change can be tracked to one reply.
/// `None` where the arm is not stated here; the registry snapshot records `null` for that command.
pub(crate) fn core_calls(id: &str) -> Option<&'static [&'static str]> {
    Some(match id {
        // Views and home state: no core call.
        "nav.home" | "nav.new" | "nav.toggle_left" | "nav.toggle_right" | "nav.toggle_panels" | "nav.ai" | "nav.tab" | "nav.page"
        | "nav.focus_request" | "view.zoom_in" | "view.zoom_out" | "view.zoom_100" | "view.zoom_fit" | "view.zoom_selection"
        | "view.zoom_set" | "view.zoom_page" | "ai.toggle_intent" | "home.view" | "home.starred_only" | "home.mode" | "home.attach" | "add.chart"
        | "add.csv" | "add.evidence" | "home.star" | "home.design_system" | "home.tile" | "home.tab" | "home.preset" | "home.runner"
        | "layer.find" | "edit.copy" | "edit.copy_properties"
        | "home.search" | "view.toggle_guides" | "view.toggle_snap" => &[] as &[&str],
        "layer.find.next" | "layer.find.previous" => &["selection.set"][..],
        "nav.open_project" => &["view.transform.set"][..],
        "nav.open_file" => &["artifact.open", "view.transform.set"][..],
        "select.node" | "select.many" | "select.clear" | "select.next" | "select.previous" => &["selection.set"][..],
        "edit.undo" => &["history.undo", "selection.set"][..],
        "edit.redo" => &["history.redo", "selection.set"][..],
        "edit.delete" => &["selection.set", "changes.apply:remove_node"][..],
        "edit.duplicate" | "edit.paste" => &["changes.apply:add_node", "changes.apply:update_figure_link", "selection.set"][..],
        "edit.paste_properties" => &["changes.apply:set_style", "selection.set"][..],
        "edit.group" | "edit.stack" => {
            &["changes.apply:add_node", "changes.apply:move_node", "changes.apply:set_geometry", "selection.set"][..]
        }
        "edit.ungroup" => &["selection.set", "changes.apply:move_node", "changes.apply:set_geometry", "changes.apply:remove_node"][..],
        "edit.order_forward" | "edit.order_backward" | "edit.order_front" | "edit.order_back" | "layer.move" => {
            &["changes.apply:set_z_order", "selection.set"][..]
        }
        "edit.lock" | "edit.unlock" => &["changes.apply:set_node_appearance", "selection.set"][..],
        "edit.hide" | "edit.show" | "edit.rename" => &["changes.apply:set_node_info", "selection.set"][..],
        "edit.nudge" => &["changes.apply:set_geometry", "selection.set"][..],
        "add.text" | "add.rect" | "add.frame" | "add.oval" | "add.line" => &["changes.apply:add_node", "selection.set"][..],
        "add.image" => &["asset.import", "changes.apply:add_node"][..],
        "add.icon" => &["changes.apply:add_node", "selection.set"][..],
        "add.path" => &["changes.apply:add_node", "selection.set"][..],
        "add.figure_image" => &["asset.import", "changes.apply:add_panel", "changes.apply:arrange_panels"][..],
        "data.edit_cell" | "data.add_column" | "data.add_row" | "data.remove_row" | "data.remove_column" => &["figure.dataset.edit"][..],
        "add.table" => &["changes.apply:insert_flow_block", "selection.set"][..],
        "canvas.table.add" => &["changes.apply:add_node", "selection.set"][..],
        "canvas.table.add_row" | "canvas.table.add_column" => &["changes.apply:add_node", "changes.apply:set_canvas_table_structure", "selection.set"][..],
        "canvas.table.remove_row" | "canvas.table.remove_column" => &["changes.apply:remove_node", "changes.apply:set_canvas_table_structure", "selection.set"][..],
        "canvas.table.set_header" => &["changes.apply:set_canvas_table_structure", "selection.set"][..],
        "canvas.table.set_column_widths" => &["changes.apply:set_canvas_table_column_widths", "selection.set"][..],
        "table.merge_cells" | "table.unmerge_cell" => &["changes.apply:set_canvas_table_structure", "selection.set"][..],
        "figure.style" => &["changes.apply:set_style"][..],
        "figure.toggle_sheet" => &[][..],
        // A dry run for the losses, then the HTML into a scratch file that the preview window shows.
        "view.play" => &["artifact.export"][..],
        "add.paragraph" => &["changes.apply:insert_flow_block"][..],
        // A page document takes the picture as a node, a text document as a flow block; the link follows in the same change.
        "add.figure_link" => &["figure.render_link", "changes.apply:add_node", "changes.apply:insert_flow_block", "changes.apply:update_figure_link", "selection.set"][..],
        "figure.update_link" => &["figure.render_link", "changes.apply:update_figure_link", "selection.set"][..],
        "figure.open_source" => &["view.transform.set"][..],
        "add.figure_panel" => &["changes.apply:add_panel", "changes.apply:arrange_panels"][..],
        "add.diagram_node" => &["changes.apply:add_diagram_node"][..],
        "figure.add_equation" => &["changes.apply:add_diagram_node"][..],
        "add.connector" => &["changes.apply:connect_diagram"][..],
        "figure.arrange_panels" => &["changes.apply:arrange_panels"][..],
        "figure.remove_panel" => &["selection.set", "changes.apply:remove_panel"][..],
        "figure.reorder_panels" => &["changes.apply:reorder_panels"][..],
        "figure.caption" => &["changes.apply:set_caption"][..],
        "figure.panel_label" => &["changes.apply:set_panel_label"][..],
        "figure.plot" => &["changes.apply:set_plot"][..],
        "figure.axis_label" => &["changes.apply:set_axis_label"][..],
        "figure.axis_ticks" => &["changes.apply:set_axis_ticks"][..],
        "figure.put_dataset" => &["changes.apply:put_dataset"][..],
        "figure.put_evidence" => &["changes.apply:put_evidence"][..],
        "figure.diagram_provenance" => &["changes.apply:set_diagram_provenance"][..],
        "figure.common_legend" => &["changes.apply:set_common_legend"][..],
        "figure.output_profile" => &["changes.apply:set_style"][..],
        "figure.visual_review" => &["changes.apply:set_figure_visual_review"][..],
        "figure.image_transform" => &["changes.apply:set_figure_image_transform"][..],
        "figure.image_calibration" => &["changes.apply:set_figure_image_calibration"][..],
        "page.remove" => &["selection.set", "changes.apply:remove_page"][..],
        "page.reorder" => &["changes.apply:reorder_pages"][..],
        "page.notes" => &["changes.apply:set_page_notes"][..],
        "slides.theme" => &["changes.apply:set_theme"][..],
        "design_theme.apply" => &["design_themes.apply"][..],
        // A change made while something is selected is followed by the window's selection refresh (see props.variant_style).
        "canvas.guides.set" | "canvas.guides.clear" => &["changes.apply:set_canvas_page_guides", "selection.set"][..],
        "page.component" => &["changes.apply:set_page_role"][..],
        "component.instantiate" => &["changes.apply:instantiate_component"][..],
        "component.detach" => &["changes.apply:detach_instance"][..],
        // Repeat and conditional groups: each command's change set, in the order its operations are first sent.
        "repeat.create" => {
            &["changes.apply:insert_page", "changes.apply:set_page_role", "changes.apply:move_node", "changes.apply:add_node", "changes.apply:set_group_repeat"][..]
        }
        "repeat.bind" | "repeat.clear" => &["changes.apply:set_group_repeat"][..],
        "repeat.condition_set" | "repeat.condition_clear" => &["changes.apply:set_group_condition"][..],
        // A change made while something is selected is followed by the window's selection refresh (measured by
        // command_calls_tests).
        "props.variant_style" => &["changes.apply:set_variant_style", "selection.set"][..],
        "props.letter_spacing" | "props.vertical_align" | "props.line_height" | "props.stroke_style" | "props.gradient"
        | "props.gradient_remove" => &["changes.apply:set_style", "selection.set"][..],
        "props.grid" | "props.grid_span" | "props.padding_sides" | "props.align_self" => &["changes.apply:set_layout", "selection.set"][..],
        "fix.grow_to_min" | "fix.grow_box" | "fix.grow_parent" | "fix.move_into_page" => &["changes.apply:set_geometry", "selection.set"][..],
        "fix.darken_text" | "fix.shrink_text" => &["changes.apply:set_style", "selection.set"][..],
        "layer.move_to" => &["changes.apply:move_node", "changes.apply:set_geometry", "selection.set"][..],
        "text.replace" => &["changes.apply:replace_text", "selection.set"][..],
        "text.style_range" => &["changes.apply:set_text_style_range", "selection.set"][..],
        "text.list" => &["changes.apply:set_node_list", "selection.set"][..],
        "doc.paragraph_format" => &["changes.apply:set_paragraph_format", "selection.set"][..],
        "doc.page_setup" => &["changes.apply:set_page_setup", "selection.set"][..],
        "doc.named_style" => &["changes.apply:set_named_style", "selection.set"][..],
        "doc.table_columns" => &["changes.apply:set_table_dimensions", "selection.set"][..],
        "web.replace_source" => &["changes.apply:replace_web_source"][..],
        "web.mapped_edit" => &["changes.apply:mapped_web_edit"][..],
        "web.open_file" => &["selection.set"][..],
        "add.page" | "add.slide" => &["changes.apply:insert_page", "selection.set"][..],
        "props.position" | "props.size" | "props.align" | "props.distribute" => &["changes.apply:set_geometry", "selection.set"][..],
        "props.image_fit" => &["changes.apply:set_node_appearance", "selection.set"][..],
        "props.image_alt" => &["changes.apply:set_node_info", "selection.set"][..],
        "props.path" => &["changes.apply:set_node_path", "selection.set"][..],
        "props.size_mode" | "props.layout_direction" | "props.layout_justify" | "props.layout_align" | "props.layout_gap" | "props.layout_padding"
        | "props.layout_mode" => &["changes.apply:set_layout", "selection.set"][..],
        "props.opacity" | "props.corner_radius" | "props.stroke_width" | "props.text_style" | "props.text_align" | "props.fill" | "props.stroke" => {
            &["changes.apply:set_style", "selection.set"][..]
        }
        "props.fill_remove" | "props.stroke_remove" => &["changes.apply:set_style", "selection.set"][..],
        "props.shadow" => &["changes.apply:set_style"][..],
        "props.shadow_remove" => &["changes.apply:set_style", "selection.set"][..],
        "props.page_size" => &["changes.apply:set_page_size"][..],
        "props.rename_page" => &["changes.apply:rename_page", "selection.set"][..],
        "canvas.open_url" => &["changes.apply:set_interactions", "selection.set"][..],
        "canvas.interaction.set" | "canvas.interaction.remove" => &["changes.apply:set_interactions", "selection.set"][..],
        "canvas.token.put" => &["changes.apply:put_token"][..],
        "canvas.token.remove" => &["changes.apply:remove_token"][..],
        "canvas.token.mode" => &["changes.apply:set_token_mode"][..],
        "canvas.token.bind" => &["changes.apply:bind_token", "selection.set"][..],
        "canvas.token.unbind" => &["changes.apply:unbind_token", "selection.set"][..],
        "canvas.design_system.list" => &["design_system.list"][..],
        "canvas.design_system.publish" => &["design_system.publish"][..],
        "canvas.design_system.set" => &["design_system.set"][..],
        "text.set" => &["changes.apply:set_text"][..],
        "figure.plot_display" => &["changes.apply:set_plot_display"][..],
        "canvas.image.replace" => &["changes.apply:replace_image_asset"][..],
        "canvas.image.crop" => &["changes.apply:set_canvas_image_crop"][..],
        "canvas.board_memos.set" => &["changes.apply:set_canvas_board_memos"][..],
        "canvas.page_sections.set" => &["changes.apply:set_canvas_page_sections"][..],
        // Dialogs, saving and exporting, jobs and the web preview: declared from the arms (not run by the test).
        "file.save" | "file.save_as" => &["artifact.save"][..],
        "file.export" | "file.dismiss_loss" | "file.export_selection" => &["artifact.export"][..],
        "web.preview" => &["web.materialize"][..],
        // With evidence files in a Figure's list, each file is imported before the job starts (figure_ai_attach.rs).
        "ai.request" | "ai.retry" => &["asset.import", "job.start"][..],
        "ai.cancel" => &["job.cancel"][..],
        "ai.undo_change" => &["history.undo_change"][..],
        "proposal.apply" => &["proposal.apply"][..],
        "ai.deliver_as" | "proposal.select" | "proposal.discard" => &[] as &[&str],
        "home.create" => &["artifact.create", "asset.import", "job.start", "view.transform.set"][..],
        "home.use_template" => &["artifact.create", "view.transform.set"][..],
        _ => return None,
    })
}

/// Commands whose several `changes.apply:` entries in `core_calls` are the operation kinds of one change set, sent in one
/// `changes.apply` call. Any other command's entries are one call each (`repeat_groups_tests` counts the calls).
/// A table row or column edit is the cells' nodes and the table's structure in one change set (`apply_canvas_table_structure`).
const ONE_CHANGE_SET: &[&str] =
    &["repeat.create", "canvas.table.add_row", "canvas.table.remove_row", "canvas.table.add_column", "canvas.table.remove_column"];

/// Whether the command changes the document with one `changes.apply` call and sends nothing else but `selection.set`, as
/// `core_calls` and `ONE_CHANGE_SET` declare it. The UI channel follows such a change to its one reply.
pub(crate) fn one_tracked_change(id: &str) -> bool {
    let Some(calls) = core_calls(id) else { return false };
    if calls == &["design_themes.apply"] { return true; }
    let changes = calls.iter().filter(|call| call.starts_with("changes.apply:")).count();
    calls.iter().all(|call| call.starts_with("changes.apply:") || *call == "selection.set")
        && (changes == 1 || (changes > 1 && ONE_CHANGE_SET.contains(&id)))
}

#[cfg(test)]
/// The registry as the checked-in snapshot records it (`crates/desktop/commands.json`), in registry order.
pub fn registry_json() -> Value {
    let commands: Vec<Value> = COMMANDS
        .iter()
        .map(|spec| {
            let ui = match ui_permission(spec) {
                UiPermission::Read => "read",
                UiPermission::Change => "change",
                UiPermission::AgentStart => "agent_start",
                UiPermission::FileWrite => "file_write",
            };
            serde_json::json!({
                "id": spec.id,
                "label": spec.label,
                "permission": format!("{:?}", spec.permission),
                "ui_permission": ui,
                "intents": ui_intents(spec),
                "shortcut": spec.shortcut,
                "wired": available(spec.id),
                "args": serde_json::from_str::<Value>(spec.args).expect("the argument schema is JSON"),
                "core_calls": core_calls(spec.id),
            })
        })
        .collect();
    serde_json::json!({ "commands": commands })
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// The checked-in snapshot of the registry, which the GUI <-> MCP parity test reads. It is rewritten only when
    /// `DESIGNLLM_COMMANDS_WRITE=1` is set; otherwise a difference from the code fails this test.
    #[test]
    fn the_registry_matches_the_checked_in_snapshot() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/commands.json");
        let mut generated = serde_json::to_string_pretty(&registry_json()).expect("the registry serialises");
        generated.push('\n');
        if std::env::var_os("DESIGNLLM_COMMANDS_WRITE").is_some() {
            std::fs::write(path, &generated).expect("the snapshot is written");
            return;
        }
        let checked_in = std::fs::read_to_string(path)
            .expect("commands.json is checked in; regenerate it with DESIGNLLM_COMMANDS_WRITE=1");
        assert!(
            checked_in == generated,
            "commands.json differs from the registry; regenerate with DESIGNLLM_COMMANDS_WRITE=1 and review the diff"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::figure_rules::{DIAGRAM_ORIGINS, IMAGE_ROTATION_LIMIT_MILLIDEG};
    use serde_json::json;
    use std::collections::{BTreeSet, HashSet};

    #[test]
    fn every_command_id_is_unique_and_dotted() {
        let mut seen = HashSet::new();
        for spec in COMMANDS {
            assert!(seen.insert(spec.id), "duplicate id {}", spec.id);
            assert!(spec.id.contains('.'), "{} is not namespaced", spec.id);
            assert!(spec.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'), "{}", spec.id);
        }
    }

    #[test]
    fn add_path_takes_path_data_and_a_view_box_as_text() {
        let spec = find("add.path").expect("the path command is registered");
        assert!(validate(spec, &json!({ "d": "M 0 0 L 24 24", "view_box": "0 0 24 24" })).is_ok());
        assert!(validate(spec, &json!({ "d": "M 0 0 L 24 24" })).unwrap_err().contains("missing `view_box`"));
        assert!(validate(spec, &json!({ "d": null, "view_box": "0 0 24 24" })).is_err(), "null path data is refused");
        assert!(validate(spec, &json!({ "d": "M 0 0", "view_box": 24 })).is_err(), "a view box must be text");
        assert_eq!(ui_permission(spec), UiPermission::Change);
    }

    #[test]
    fn design_system_set_schema_accepts_a_source_or_null_detach() {
        let spec = find("canvas.design_system.set").expect("the shared source command is registered");
        assert!(validate(spec, &json!({ "design_system": "publisher-id", "expected_system_revision": 4 })).is_ok());
        assert!(validate(spec, &json!({ "design_system": null })).is_ok());
        assert!(validate(spec, &json!({ "design_system": "" })).is_err());
        assert!(validate(spec, &json!({ "design_system": 3 })).is_err());
        assert!(validate(spec, &json!({})).is_err());
        assert!(validate(spec, &json!({ "design_system": "publisher-id", "direct_formatting": "keep" })).is_ok());
        assert!(validate(spec, &json!({ "design_system": "publisher-id", "direct_formatting": "clear" })).is_ok());
        assert!(validate(spec, &json!({ "design_system": "publisher-id", "direct_formatting": "drop" })).is_err());
    }

    #[test]
    fn every_argument_schema_is_a_json_object_schema() {
        for spec in COMMANDS {
            let schema: Value = serde_json::from_str(spec.args).unwrap_or_else(|error| panic!("{}: {error}", spec.id));
            assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"), "{}", spec.id);
        }
    }

    #[test]
    fn every_shortcut_parses_and_no_two_commands_share_one() {
        let mut seen = BTreeSet::new();
        for spec in COMMANDS.iter().filter(|spec| !spec.shortcut.is_empty()) {
            assert!(keyboard_shortcut(spec).is_some(), "{} has an unreadable shortcut {}", spec.id, spec.shortcut);
            let shortcut = keyboard_shortcut(spec).unwrap();
            let duplicate = !seen.insert((shortcut.modifiers.command, shortcut.modifiers.shift, shortcut.modifiers.alt, shortcut.logical_key));
            assert!(!duplicate, "{} repeats the shortcut {}", spec.id, spec.shortcut);
        }
    }

    #[test]
    fn the_registry_has_every_permission_class_and_enough_commands() {
        assert!(COMMANDS.len() >= 100, "found {}", COMMANDS.len());
        for permission in [Permission::Read, Permission::Design, Permission::Content, Permission::FileWrite] {
            assert!(COMMANDS.iter().any(|spec| spec.permission == permission), "no {permission:?} command");
        }
    }

    #[test]
    fn required_arguments_and_enums_are_checked_before_anything_runs() {
        let set = find("props.fill").unwrap();
        assert!(validate(set, &json!({"color": "#1E7F4F"})).is_ok());
        assert!(validate(set, &json!({})).unwrap_err().contains("missing `color`"));
        assert!(validate(set, &json!({"color": "green"})).is_err());
        let align = find("props.align").unwrap();
        assert!(validate(align, &json!({"edge": "diagonal"})).is_err());
        assert!(validate(find("view.zoom_set").unwrap(), &json!({"percent": 140})).is_err());
        assert!(validate(find("nav.page").unwrap(), &json!({"page_index": 1.5})).is_err());
        assert!(validate(find("nav.page").unwrap(), &json!({"page_index": 2})).is_ok());
    }

    #[test]
    fn the_channel_classes_follow_the_registry_classes() {
        assert_eq!(ui_permission(find("view.zoom_in").unwrap()), UiPermission::Read);
        assert_eq!(ui_permission(find("props.fill").unwrap()), UiPermission::Change);
        assert_eq!(ui_permission(find("file.save").unwrap()), UiPermission::FileWrite);
        assert_eq!(ui_permission(find("ai.request").unwrap()), UiPermission::AgentStart);
        assert!(ui_intents(find("edit.delete").unwrap()).contains(&Intent::Content));
        assert!(ui_intents(find("props.fill").unwrap()).contains(&Intent::Design));
        assert!(ui_intents(find("view.zoom_in").unwrap()).is_empty());
        assert!(ui_intents(find("ai.request").unwrap()).is_empty(), "starting a job changes nothing by itself");
        assert!(!available("web.refresh"));
        assert!(available("edit.undo"));
        // The AI intent switch turns on any intent, so an agent run needs a change class and every intent.
        let toggle = find("ai.toggle_intent").unwrap();
        assert_eq!(ui_permission(toggle), UiPermission::Change);
        let all: BTreeSet<Intent> = [Intent::Design, Intent::Content, Intent::Data].into_iter().collect();
        assert_eq!(ui_intents(toggle), all);
    }

    #[test]
    fn every_unwired_command_is_a_registered_command() {
        for id in NOT_WIRED {
            assert!(find(id).is_some(), "{id} is not in the registry");
        }
    }

    #[test]
    fn figure_plot_scales_are_the_engine_names() {
        let plot = find("figure.plot").unwrap();
        assert!(validate(plot, &json!({ "x_scale": "log10", "y_scale": "linear" })).is_ok());
        assert!(validate(plot, &json!({ "x_scale": "log" })).is_err(), "the engine reads log10, not log");
    }

    #[test]
    fn figure_rule_schema_literals_match_the_contract_constants() {
        // The schema strings are literals, so the contract constants are compared with them here. The rotation bound
        // is exclusive (360 degrees and more is refused), so the schema stops one millidegree short of it.
        let transform: Value = serde_json::from_str(find("figure.image_transform").unwrap().args).unwrap();
        let rotation = &transform["properties"]["rotation_millidegrees"];
        assert_eq!(rotation["minimum"], json!(-(IMAGE_ROTATION_LIMIT_MILLIDEG - 1)));
        assert_eq!(rotation["maximum"], json!(IMAGE_ROTATION_LIMIT_MILLIDEG - 1));
        let provenance: Value = serde_json::from_str(find("figure.diagram_provenance").unwrap().args).unwrap();
        assert_eq!(provenance["properties"]["origin"]["enum"], json!(DIAGRAM_ORIGINS));
    }

    #[test]
    fn array_and_null_types_and_array_items_are_checked() {
        // Regression: a type list (`["array","null"]`) and the items of an array were not checked at all.
        let widths = find("canvas.table.set_column_widths").unwrap();
        assert!(validate(widths, &json!({ "table_id": "t", "column_widths": [1, 2] })).is_ok());
        assert!(validate(widths, &json!({ "table_id": "t", "column_widths": null })).is_ok());
        assert!(validate(widths, &json!({ "table_id": "t", "column_widths": "1,2" })).is_err(), "a string is not a list");
        assert!(validate(widths, &json!({ "table_id": "t", "column_widths": [0] })).is_err(), "an item below its minimum");
        assert!(validate(widths, &json!({ "table_id": "t", "column_widths": [1, "2"] })).is_err(), "an item of another type");
        assert!(validate(widths, &json!({ "table_id": "t", "column_widths": [1.5] })).is_err());
        assert!(validate(widths, &json!({ "table_id": null, "column_widths": [1] })).is_err(), "a plain string does not take null");
        assert!(validate(find("select.many").unwrap(), &json!({ "node_ids": ["a", 3] })).is_err());
        assert!(validate(find("select.many").unwrap(), &json!({ "node_ids": "a" })).is_err());
    }

    #[test]
    fn toggle_intent_takes_a_mode_or_an_intent_and_refuses_unknown_values() {
        let toggle = find("ai.toggle_intent").unwrap();
        assert!(validate(toggle, &json!({ "mode": "design_only" })).is_ok(), "a mode alone is enough");
        assert!(validate(toggle, &json!({ "mode": "both" })).is_ok());
        assert!(validate(toggle, &json!({ "intent": "data" })).is_ok());
        assert!(validate(toggle, &json!({ "intent": "design", "mode": "design_only" })).is_ok(), "the window sends both");
        assert!(validate(toggle, &json!({})).is_err(), "one of the two is required");
        assert!(validate(toggle, &json!({ "mode": "design" })).is_err(), "an unknown mode is refused");
        assert!(validate(toggle, &json!({ "intent": "image" })).is_err());
        // A null is absent: a null mode leaves no argument set, and a null intent leaves the mode alone.
        assert!(validate(toggle, &json!({ "mode": null })).is_err(), "a null mode is absent, so no argument set is given");
        assert!(validate(toggle, &json!({ "mode": null, "intent": "design" })).is_ok(), "the intent alone is given");
        assert!(validate(toggle, &json!({ "intent": null, "mode": "both" })).is_ok(), "the mode alone is given");
    }

    #[test]
    fn a_null_for_an_optional_property_is_absent_for_the_check_and_the_handler() {
        let export = find("file.export").unwrap();
        // The agent channel sends null for each optional field it leaves out.
        let sent: Value = serde_json::from_str(
            r#"{"format":"png","target_path":"out.png","overwrite":null,"expected_target_hash":null,"allowed_losses":null,"scale":null,"background":null,"scope":null}"#,
        )
        .unwrap();
        assert!(validate(export, &sent).is_ok());
        assert!(validate(export, &json!({ "format": "png", "target_path": "out.png", "scale": 4 })).is_err(), "a value that is not null is still checked");
        assert!(validate(find("props.position").unwrap(), &json!({ "x": null, "y": 2 })).is_ok());
        assert_eq!(without_absent_nulls(export, sent), json!({ "format": "png", "target_path": "out.png" }));
    }

    #[test]
    fn a_null_for_a_required_property_is_refused_unless_its_schema_lists_null() {
        let export = find("file.export").unwrap();
        assert!(validate(export, &json!({ "format": null, "target_path": "out.png" })).is_err());
        assert!(validate(export, &json!({ "format": "png", "target_path": null })).is_err());
        let kept = without_absent_nulls(export, json!({ "format": null, "target_path": "out.png" }));
        assert!(kept.get("format").is_some(), "a required null is not removed");
        assert!(validate(export, &kept).is_err());
        // The design system detach is a required null whose schema lists null.
        let set = find("canvas.design_system.set").unwrap();
        assert!(validate(set, &json!({ "design_system": null })).is_ok());
        assert_eq!(without_absent_nulls(set, json!({ "design_system": null })), json!({ "design_system": null }));
    }

    #[test]
    fn a_null_axis_limit_keeps_its_value_because_null_clears_the_limit() {
        // figure.plot reads a present null as "clear the limit"; an absent key would keep the old limit.
        let plot = find("figure.plot").unwrap();
        assert!(validate(plot, &json!({ "y_minimum": null, "y_maximum": null })).is_ok());
        assert_eq!(
            without_absent_nulls(plot, json!({ "panel_id": "p", "y_minimum": null, "y_scale": null })),
            json!({ "panel_id": "p", "y_minimum": null })
        );
        let widths = find("canvas.table.set_column_widths").unwrap();
        assert_eq!(
            without_absent_nulls(widths, json!({ "table_id": "t", "column_widths": null })),
            json!({ "table_id": "t", "column_widths": null }),
            "the reset to equal widths is a required null that the schema lists"
        );
    }

    #[test]
    fn a_figure_value_that_null_clears_is_required_and_its_null_reaches_the_handler() {
        // 49zf: the operations behind these commands refuse an omitted value, so the commands do too, and a null is sent on.
        let ticks = find("figure.axis_ticks").unwrap();
        assert!(validate(ticks, &json!({ "axis": "y" })).is_err(), "an omitted tick step is refused");
        assert!(validate(ticks, &json!({ "axis": "y", "tick_step": 0.5 })).is_ok());
        assert_eq!(without_absent_nulls(ticks, json!({ "axis": "y", "tick_step": null })), json!({ "axis": "y", "tick_step": null }));
        for (id, key) in [
            ("figure.diagram_provenance", "origin"),
            ("figure.common_legend", "legend"),
            ("figure.visual_review", "review"),
            ("figure.image_transform", "crop"),
            ("figure.image_calibration", "calibration"),
        ] {
            let schema: Value = serde_json::from_str(find(id).unwrap().args).unwrap();
            assert!(schema["required"].as_array().unwrap().contains(&json!(key)), "{id}: `{key}` is required");
            let mut sent = json!({ "element_id": "n", "evidence_ids": [], "rotation_millidegrees": 0 });
            sent[key] = Value::Null;
            assert_eq!(without_absent_nulls(find(id).unwrap(), sent.clone())[key], Value::Null, "{id}: the null `{key}` stays");
        }
    }

    #[test]
    fn stripping_reaches_declared_object_properties_and_leaves_undeclared_keys_alone() {
        const NESTED: CommandSpec = CommandSpec {
            id: "test.nested",
            label: "",
            permission: Permission::Content,
            shortcut: "",
            args: r#"{"type":"object","properties":{"outer":{"type":"object","properties":{"inner":{"type":"string"}}}}}"#,
        };
        assert_eq!(
            without_absent_nulls(&NESTED, json!({ "outer": { "inner": null, "extra": null }, "other": null })),
            json!({ "outer": { "extra": null }, "other": null })
        );
    }

    #[test]
    fn nulls_of_optional_fields_in_alternatives_and_array_items_are_absent_too() {
        // Regression: only the top-level properties were stripped, so the null `legend.placement` (declared inside a `oneOf`
        // alternative) reached `figure.r07`, which refuses it.
        let legend = find("figure.common_legend").unwrap();
        let sent = json!({ "legend": { "items": [{ "label": "a", "color": "#112233" }], "placement": null, "title": null } });
        assert!(validate(legend, &sent).is_ok());
        assert_eq!(without_absent_nulls(legend, sent), json!({ "legend": { "items": [{ "label": "a", "color": "#112233" }] } }));
        // The legend itself may be cleared by null, and a null for a required property stays for `validate` to refuse.
        let cleared = json!({ "legend": null });
        assert_eq!(without_absent_nulls(legend, cleared.clone()), cleared);
        let required = json!({ "legend": { "items": [{ "label": null, "color": "#112233" }] } });
        assert!(validate(legend, &required).is_err());
        assert_eq!(without_absent_nulls(legend, required.clone()), required);
    }

    #[test]
    fn letters_are_bare_keys_that_must_not_fire_in_a_text_field() {
        assert!(is_bare_key(find("add.text").unwrap()));
        assert!(!is_bare_key(find("edit.undo").unwrap()));
        assert!(!is_bare_key(find("view.play").unwrap()));
    }
}
