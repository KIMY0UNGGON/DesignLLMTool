use super::*;

/// The sentence for a failed call. A change the figure engine refused names its reason through a
/// diagnostic code, which says more than the error code (every refusal is "unsupported").
pub(crate) fn user_error(error: &DltError) -> String {
    if let Some(message) = error
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.blocking)
        .find_map(|diagnostic| diagnostic_message(&diagnostic.code))
    {
        return message.into();
    }
    match &error.code {
        ErrorCode::Unavailable => "잠시 후 다시 시도해 주세요".into(),
        ErrorCode::InvalidArgument => "입력 내용을 확인해 주세요".into(),
        ErrorCode::NotFound => "작업물이나 파일을 찾을 수 없습니다".into(),
        ErrorCode::AlreadyExists => "해당 경로에 파일이 있습니다. 새 경로를 선택해 주세요".into(),
        ErrorCode::RequestIdMismatch => "요청을 확인할 수 없습니다. 다시 시도해 주세요".into(),
        ErrorCode::TargetBusy => "다른 편집이 진행 중입니다".into(),
        ErrorCode::WorkspaceLocked => "작업공간이 다른 창에서 사용 중입니다".into(),
        ErrorCode::HostNotRunning => "작업공간에 연결할 수 없습니다".into(),
        ErrorCode::ProtocolMismatch => "작업공간 서비스 버전이 맞지 않습니다".into(),
        ErrorCode::RenderPending => "미리보기를 만드는 중입니다".into(),
        ErrorCode::StaleRender => "최신 미리보기를 다시 불러오는 중입니다".into(),
        ErrorCode::RevisionConflict => "다른 수정이 먼저 저장됐습니다. 최신 내용을 다시 불러왔습니다".into(),
        ErrorCode::UnsupportedOperation => "이 문서에서는 이 작업을 지원하지 않습니다".into(),
        ErrorCode::ContentLocked => "보호된 내용은 수정할 수 없습니다".into(),
        ErrorCode::DataAmbiguous => "숫자 형식을 해석할 수 없어 변경하지 않았습니다".into(),
        ErrorCode::SourceChanged => "원본 파일이 바뀌었습니다. 다시 열어 주세요".into(),
        ErrorCode::UnsafeRoundtrip => "이 변경을 안전하게 저장할 수 없습니다".into(),
        ErrorCode::ExportLossRequired => "내보내기에 손실 가능성이 있어 완료하지 않았습니다".into(),
        ErrorCode::Canceled => "작업이 취소됐습니다".into(),
        ErrorCode::Io => "파일에 접근하지 못했습니다. 경로와 권한을 확인해 주세요".into(),
        ErrorCode::Internal | ErrorCode::StorageFailure | ErrorCode::IntegrityError => {
            "작업을 완료하지 못했습니다. 작업공간을 확인한 뒤 다시 시도해 주세요".into()
        }
        ErrorCode::JobNotActive => "이 AI 작업은 더 이상 활성 상태가 아닙니다".into(),
        ErrorCode::SchemaUnsupported => "이 작업물 버전은 현재 앱에서 열 수 없습니다".into(),
    }
}

/// How the window shows a diagnostic code: the sentence of a refusal (a blocking diagnostic), the sentence of a
/// warning (a non-blocking one), or nothing (a note that is not for people).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Words {
    Refusal(&'static str),
    Warning(&'static str),
    Silent,
}

/// The one table of the window's words for engine diagnostic codes. A code that an engine reports and that the window
/// words is a contracts constant (`diag_codes`, `repeat_groups`, the PDF codes); the engine reports the same constant.
pub(super) fn diagnostic_words(code: &str) -> Option<Words> {
    use designllm_contracts::{diag_codes as codes, repeat_groups as repeat};
    use designllm_contracts::{PDF_EDIT_APPROXIMATE, PDF_EDIT_FAILED, PDF_EDIT_NO_TEXT, PDF_EDIT_WORD_REQUIRED};
    use Words::{Refusal, Silent, Warning};
    Some(match code {
        PDF_EDIT_APPROXIMATE => Warning("PDF를 편집 가능한 문서로 변환했습니다. 표와 쪽 나눔을 확인해 주세요"),
        PDF_EDIT_WORD_REQUIRED => Refusal("PDF를 편집하려면 이 PC에 Microsoft Word가 설치되어 있어야 합니다"),
        PDF_EDIT_NO_TEXT => Refusal("이 PDF에서 편집 가능한 글자를 찾지 못했습니다"),
        PDF_EDIT_FAILED => Refusal("PDF를 편집 가능한 문서로 변환하지 못했습니다"),
        "core.workspace_path_too_long" => Refusal(
            "작업공간 경로가 너무 깁니다. 더 짧은 폴더 경로로 옮긴 뒤 다시 여세요. 이번 시도에서는 데이터베이스와 기존 백업이 변경되지 않았습니다",
        ),
        "core.workspace_path_ambiguous" => Refusal(
            "긴 Windows 작업공간 경로에 '.' 또는 '..' 구성요소가 있어 안전하게 열 수 없습니다. 경로를 정규화한 뒤 다시 선택하세요. 이번 시도에서는 데이터베이스와 기존 백업이 변경되지 않았습니다",
        ),
        codes::DOCX_DIGITAL_SIGNATURE_UNSUPPORTED | "pptx.digital_signature_unsupported" => Refusal("전자 서명된 문서는 지원하지 않아 처리하지 않았습니다"),
        codes::DOCX_COMPOUND_CONTAINER_UNSUPPORTED | "pptx.compound_container_unsupported" => {
            Refusal("암호화된 문서이거나 지원하지 않는 이전 Office 형식이라 열 수 없습니다")
        }
        "hwp.protected_document_unsupported" => Refusal("암호나 배포·DRM 보호가 걸린 HWP 문서는 열 수 없습니다"),
        "hwp.digital_signature_unsupported" => Refusal("전자 서명된 문서는 지원하지 않아 처리하지 않았습니다"),
        "PPTX_OPC_ERROR" => Refusal("파일 구조를 확인할 수 없어 처리하지 않았습니다"),
        codes::PANEL_OVERLAP => Refusal("패널이 서로 겹칩니다"),
        codes::PANEL_OUTSIDE_CANVAS => Refusal("패널이 그림 영역을 벗어납니다"),
        codes::PANEL_SIZE_INVALID | codes::PANEL_TOO_SMALL => Refusal("패널이 너무 작습니다"),
        codes::NODE_OUTSIDE_PANEL => Refusal("도형이 패널 밖으로 나갑니다"),
        codes::NODE_SIZE_INVALID => Refusal("도형의 크기는 0보다 커야 합니다"),
        codes::NODE_KIND_UNSUPPORTED => Refusal("구조도에는 사각형과 원만 넣을 수 있습니다"),
        codes::LABEL_OVERFLOW => Refusal("글자가 도형 안에 들어가지 않습니다. 도형을 키우거나 글자를 줄여 주세요"),
        codes::LABEL_OUTSIDE_PANEL => Refusal("축 글자가 패널 밖으로 나갑니다"),
        codes::LABEL_COLLISION => Refusal("글자를 겹치지 않게 놓을 공간이 부족합니다. 주변 항목을 옮기거나 패널을 키워 주세요"),
        codes::LEGEND_OVERFLOW => Refusal("글자가 서로 겹칩니다. 패널을 키워 주세요"),
        codes::CONNECTOR_CROSSES_NODE | codes::CONNECTOR_CROSSES_LABEL => {
            Refusal("연결선이 다른 도형을 지나갑니다. 도형 위치를 옮겨 주세요")
        }
        codes::EDGE_ZERO_LENGTH | codes::SELF_LOOP_UNSUPPORTED => Refusal("서로 다른 두 도형만 연결할 수 있습니다"),
        codes::UNIT_REQUIRED => Refusal("축 단위를 입력해 주세요"),
        "CANVAS_GUIDES_AREA" => Refusal("안내선 간격과 여백이 페이지에 들어가지 않습니다. 간격과 여백을 줄여 주세요"),
        repeat::CANVAS_REPEAT_GROUP_NOT_EMPTY => Refusal("반복 그룹 안에 항목이 있으면 반복을 만들 수 없습니다. 그룹을 비운 뒤 다시 시도하세요"),
        repeat::CANVAS_REPEAT_CONDITION_SCOPE => Refusal("조건은 반복할 컴포넌트 페이지 안의 그룹에만 설정할 수 있습니다"),
        repeat::CANVAS_REPEAT_LIMIT => Refusal("데이터 표나 펼친 결과가 허용 크기를 넘습니다. 행·열 수를 줄여 주세요"),
        repeat::CANVAS_REPEAT_STALE => Refusal("저장된 반복 결과가 데이터 표와 맞지 않습니다. 반복을 다시 설정해 주세요"),
        repeat::CANVAS_REPEAT_FLATTENED => Refusal("내보내기에는 반복이 펼친 결과로만 들어가고 데이터 연결은 남지 않습니다"),
        repeat::CANVAS_REPEAT_TARGET => Refusal("반복은 행·열·격자 배치의 빈 그룹에만 만들 수 있습니다"),
        repeat::CANVAS_REPEAT_TABLE => Refusal("이 표는 반복의 데이터 원본으로 쓸 수 없습니다. 병합되지 않은 표를 고르세요"),
        repeat::CANVAS_REPEAT_NESTED => Refusal("반복 안에는 다른 반복을 넣을 수 없습니다"),
        repeat::CANVAS_REPEAT_COLUMN => Refusal("표에 없는 열 번호입니다. 열 번호를 확인하세요"),
        repeat::CANVAS_REPEAT_BINDING => Refusal("연결할 텍스트 항목이나 셀을 다시 확인하세요"),
        repeat::CANVAS_REPEAT_CONDITION => Refusal("비교 값을 확인하세요. 같음과 다름은 값이 필요하고 비었음과 있음은 값을 쓰지 않습니다"),
        repeat::CANVAS_REPEAT_EXPECTED => Refusal("다른 수정이 먼저 반영됐습니다. 최신 내용을 다시 불러온 뒤 시도하세요"),
        codes::COLUMN_TYPE_INVALID | codes::COLUMN_REQUIRED => Refusal("이 차트에 쓸 수 없는 열입니다"),
        codes::DATASET_NOT_FOUND | codes::DATASET_REF_STALE => Refusal("자료가 바뀌었습니다. 다시 시도해 주세요"),
        codes::DATA_AMBIGUOUS | codes::DATA_MALFORMED => Refusal("숫자 형식을 해석할 수 없습니다"),
        codes::NO_PLOTTABLE_DATA => Refusal("차트로 그릴 수 있는 값이 없습니다"),
        "PDF_TYPE1_ENCODING_UNREADABLE" => Warning("일부 내장 글꼴 인코딩을 읽지 못해 기본 인코딩으로 해석했습니다. 추출된 글자를 확인해 주세요"),
        "PDF_LINE_GAP_AMBIGUOUS" => Warning("일부 줄 간격이 애매해 단 구분을 확인해 주세요"),
        "PDF_XOBJECT_MISSING" => Warning("일부 누락된 PDF 요소를 건너뛰고 읽었습니다"),
        designllm_contracts::PDF_MEMORY_LIMIT => Refusal("문서가 읽기 메모리 상한을 넘어 읽지 않았습니다"),
        "PDF_REQUEST_BUDGET" => Refusal("문서를 읽는 시간이나 작업량 상한을 넘어 읽지 않았습니다"),
        "PDF_STACK_LIMIT" => Refusal("문서의 구조가 너무 깊어 읽지 않았습니다"),
        "PDF_INTERNAL_PANIC" => Refusal("문서를 읽는 중 처리하지 못한 오류가 나 읽지 않았습니다"),
        "PDF_PARSE_ERROR" => Refusal("PDF 구조를 해석할 수 없어 읽지 않았습니다"),
        "PDF_TOO_LARGE" => Refusal("PDF 파일이 지원하는 크기를 넘어 읽지 않았습니다"),
        codes::EVIDENCE_KIND_INVALID
        | codes::EVIDENCE_GEOMETRY_INVALID
        | codes::EVIDENCE_REGION_OUTSIDE_CROP
        | codes::EVIDENCE_PAGE_NOT_IN_SCOPE
        | codes::EVIDENCE_HASH_MISMATCH
        | codes::EVIDENCE_TEXT_EMPTY
        | codes::EVIDENCE_REVIEW_STATE_INVALID => Refusal("이 근거를 그림에 추가할 수 없습니다"),
        "IMAGE_ANIMATION_UNSUPPORTED" => Refusal("애니메이션 GIF는 가져올 수 없습니다. 움직이지 않는 이미지를 고르세요"),
        "IMAGE_CORRUPT" => Refusal("이미지 파일이 손상됐거나 끝까지 읽히지 않습니다. 다른 파일을 고르세요"),
        codes::COMMON_LEGEND_INVALID => Refusal("공통 범례의 항목 이름과 색을 확인해 주세요. 색은 #RRGGBB 또는 #RRGGBBAA 형식이어야 합니다"),
        codes::COMMON_LEGEND_OVERFLOW => Refusal("공통 범례가 그림 밖으로 나갑니다. 패널을 줄이거나 범례 위치를 바꿔 주세요"),
        codes::VISUAL_REVIEW_NOT_APPROVED => Refusal("그림의 시각 검토를 승인해야 내보낼 수 있습니다"),
        // The approval's digest covers the figure's content, data and design, so any of them makes it stale.
        codes::VISUAL_REVIEW_STALE => Refusal("검토 후 그림의 내용·자료·디자인이 바뀌었습니다 — 다시 검토 필요"),
        codes::IMAGE_ROTATION_INVALID => Refusal("회전은 -360도보다 크고 360도보다 작아야 합니다"),
        codes::CALIBRATION_INVALID => Refusal("측정 보정에는 0보다 큰 길이와 픽셀 간격, 단위가 필요합니다"),
        codes::GENERATED_IMAGE_CALIBRATION => Refusal("생성 삽화에는 측정 보정을 둘 수 없습니다"),
        "table_border_overflow" => Refusal("표 테두리 선의 절반이 쪽 가장자리 밖으로 나갑니다"),
        "table_text_overflow" => Refusal("표 글자가 셀 밖으로 나갑니다"),
        "WEB_ATTR_FORBIDDEN" => Refusal("이 속성은 매핑을 통해 편집할 수 없습니다"),
        "WEB_URL_SCHEME" => Refusal("허용되지 않은 주소 형식이 있어 적용하지 않았습니다. 상대 주소와 http, https, mailto, tel 주소, 그림의 data 주소만 쓸 수 있습니다"),
        "WEB_STYLE_SYNTAX" => Refusal("스타일 값에는 CSS 주석, 역슬래시, 탭 외의 제어 문자, 닫히지 않은 문자열, 따옴표 없는 url() 안의 따옴표를 쓸 수 없습니다"),
        "WEB_ATTR_NO_VALUE" => Refusal("값이 없는 속성(예: disabled)은 값을 바꿀 수 없습니다"),
        "WEB_TOKEN_ID" => Refusal("디자인 토큰 이름은 a-z, 0-9, '-'만 쓸 수 있습니다"),
        "WEB_TOKEN_FONT" => Refusal("글꼴 토큰 값에는 제어 문자를 쓸 수 없습니다"),
        "WEB_ENTRY_MISSING" => Refusal("시작 HTML 파일이 프로젝트에 없습니다"),
        "WEB_ENTRY_PATH" => Refusal("시작 파일 경로가 올바르지 않습니다"),
        "WEB_ENTRY_TYPE" => Refusal("시작 파일은 HTML 파일이어야 합니다"),
        "WEB_NO_ENTRY" => Refusal("열 수 있는 HTML 파일이 없습니다"),
        "WEB_HEAD_AMBIGUOUS" => Refusal("시작 HTML에는 head 요소가 하나만 있어야 합니다"),
        "WEB_HEAD_MISSING" => Refusal("디자인 토큰을 연결하려면 시작 HTML에 닫는 태그가 있는 head 요소가 필요합니다"),
        "WEB_LINK_PRESENT" => Refusal("시작 HTML이 디자인 토큰 파일을 이미 연결하고 있는데 디자인 시스템이 연결되어 있지 않습니다"),
        "WEB_LINK_COUNT" => Refusal("시작 HTML은 디자인 토큰 파일을 정확히 한 번 연결해야 합니다"),
        "WEB_TOKEN_DRIFT" => Refusal("디자인 토큰 파일이 고정된 디자인 시스템 토큰과 다릅니다"),
        "WEB_GENERATED_MISSING" => Refusal("연결된 프로젝트에 디자인 토큰 파일이 없습니다"),
        "WEB_NAME_CLASH" => Refusal("디자인 시스템이 바꿀 수 없는 프로젝트 파일과 이름이 같습니다. 이름을 바꾸거나 먼저 지워 주세요"),
        "WEB_HTML_UNSUPPORTED" => Refusal("이 HTML 구조는 지원하지 않아 처리하지 않았습니다"),
        "WEB_INLINE_UNSAFE" => Refusal("닫는 태그가 들어 있는 파일은 HTML 안에 포함할 수 없습니다"),
        "WEB_BINARY_RESOURCE" => Refusal("웹 소스 프로젝트는 이진 파일을 지원하지 않습니다"),
        "WEB_FORMAT" => Refusal("지원하지 않는 웹 소스 형식입니다. ZIP 폴더, HTML 파일, DesignLLM 프로젝트만 열 수 있습니다"),
        "WEB_ENCODING" => Refusal("HTML 입력은 UTF-8이어야 합니다"),
        "WEB_BOM" => Refusal("내용은 UTF-8 BOM으로 시작할 수 없습니다"),
        "WEB_DIRECT_FORMATTING" => Refusal("웹 소스에는 지울 직접 서식이 없어 처리하지 않았습니다"),
        "WEB_PATH" => Refusal("파일 경로를 쓸 수 없습니다. 상대 경로와 허용된 이름을 확인해 주세요"),
        "WEB_TOO_MANY_FILES" => Refusal("프로젝트의 파일 수가 너무 많습니다"),
        "WEB_HASH_STALE" => Refusal("저장된 해시가 파일 내용과 맞지 않습니다. 파일이 바뀌었을 수 있습니다"),
        "WEB_FILE_TOO_LARGE" => Refusal("파일이 파일별 크기 상한을 넘습니다"),
        "WEB_TOTAL_TOO_LARGE" => Refusal("프로젝트가 전체 크기 상한을 넘습니다"),
        "WEB_NO_CHANGE" => Refusal("바뀐 내용이 없어 적용하지 않았습니다"),
        "WEB_PROPERTY" => Refusal("지원하지 않는 속성입니다. 속성 이름은 소문자, 숫자, -, _만 쓸 수 있고 항목은 text 또는 attr:<이름>이어야 합니다"),
        "WEB_MAPPING_NOT_FOUND" => Refusal("선택한 매핑이 없습니다"),
        "WEB_MAPPING_AMBIGUOUS" => Refusal("같은 매핑이 여러 곳에 있어 편집할 위치를 정할 수 없습니다"),
        "WEB_MAPPING_KEY" => Refusal("매핑 속성은 직접 편집할 수 없습니다"),
        "WEB_ATTR_MISSING" => Refusal("이 매핑에는 해당 속성이 없습니다"),
        "WEB_MAP_VOID" => Refusal("빈 요소나 자체 닫힘 요소에는 글자가 없어 편집할 수 없습니다"),
        "WEB_MAP_RAW_TEXT" => Refusal("script와 style의 본문은 매핑으로 편집할 수 없습니다"),
        "WEB_MAP_CHILDREN" => Refusal("요소 안에 다른 마크업이 있어 글자를 한 덩어리로 편집할 수 없습니다"),
        // The codes without the WEB_ prefix that the web engine shares with the canvas engine (crates/canvas/src/web, canvas).
        "ARTIFACT_ID" => Refusal("내보내기 요청이 다른 작업물을 가리켜 처리하지 않았습니다"),
        "DESIGN_SYSTEM_BODY" => Refusal("이 작업물에는 공유 디자인 토큰을 쓸 수 없습니다"),
        "EXPORT_FORMAT" => Refusal("이 작업물은 그 형식으로 내보낼 수 없습니다"),
        "KIND" => Refusal("이 종류의 작업물은 이 문서 형식으로 만들거나 열 수 없습니다"),
        "KIND_MISMATCH" => Refusal("작업물 내용이 문서 종류와 맞지 않아 처리하지 않았습니다"),
        "OPEN_ASSET_HASH" => Refusal("가져온 파일 내용이 기록과 맞지 않아 열지 않았습니다"),
        "PAGE_SELECTION" => Refusal("내보낼 쪽 선택이 올바르지 않습니다"),
        "PROJECT_KIND" => Refusal("이 문서 형식의 프로젝트 파일이 아니라 열 수 없습니다"),
        "SAVE_FILE" => Refusal("저장한 파일 하나가 프로젝트 내용과 다릅니다. 저장 결과를 확인해 주세요"),
        "SAVE_MISMATCH" => Refusal("저장한 파일을 다시 읽은 내용이 작업물과 다릅니다. 저장 결과를 확인해 주세요"),
        "SAVE_UNREADABLE" => Refusal("저장한 파일을 다시 읽어 확인하지 못했습니다. 저장 결과를 확인해 주세요"),
        "SIZE" => Refusal("애니메이션 프레임 크기가 올바르지 않습니다"),
        "TEMPLATE" => Refusal("고른 템플릿을 쓸 수 없습니다"),
        "original_bytes" | "no_effect" | "snapshot_large" | "hwp_save_verified" => Silent,
        "PPTX_LOSS" => Warning("일부 요소를 저장 형식에서 완전히 표현하지 못했습니다. 결과를 확인해 주세요."),
        "PPTX_OPC_WARNING" => Warning("PowerPoint 파일 구조 일부를 확인하지 못했습니다. 결과를 확인해 주세요."),
        // The old lower-case values (LEGACY_*) can still be stored in an artifact saved before 49f.
        codes::NATIVE_UNIT_ROUNDING | codes::LEGACY_NATIVE_UNIT_ROUNDING => Warning("일부 값이 저장 형식의 정밀도에 맞게 조정되었습니다."),
        codes::FONT_SUBSTITUTED | codes::LEGACY_FONT_SUBSTITUTED => Warning("일부 글꼴을 사용할 수 없어 대체 글꼴로 표시했습니다."),
        codes::SLIDES_THEME_KEPT_AS_SET => Warning("글자를 나눠 고친 도형은 새 테마를 따르지 않고 지금 색과 글꼴을 그대로 두었습니다."),
        "SLIDES_STYLE_IGNORED" => Warning("그림의 기존 모양은 유지되고 새 모양은 적용되지 않았습니다."),
        "hwp_line_spacing_mapped" => Warning("줄 간격이 한글 문서 형식에 맞게 조정되었습니다."),
        "hwp_new_paragraph_style" => Warning("새 문단은 앞 문단의 글꼴과 문단 모양을 이어받습니다."),
        "hwp_table_trailing_paragraph" => Warning("한글 문서의 표 뒤에 빈 문단이 추가될 수 있습니다."),
        "hwp_picture_crop_reset" => Warning("일부 그림은 미리보기에서 자르기 없이 표시됩니다."),
        codes::HWPX_PREVIEW_TEXT_REGENERATED => Warning("문서 미리보기 텍스트를 편집 내용에 맞게 갱신했습니다."),
        codes::HWPX_UNMODELED_PRESERVED => Warning("지원하지 않는 문서 요소는 원본 그대로 보존했습니다."),
        codes::HWPX_FLOATING_OBJECT_NOT_RENDERED => Warning("일부 떠 있는 개체는 미리보기에 표시되지 않습니다."),
        codes::HWPX_LAYOUT_APPROXIMATE => Warning("페이지 배치는 한글 앱과 다를 수 있습니다."),
        codes::DOCX_PROTECTED_INLINE | codes::DOCX_PROTECTED_BLOCK => Warning("보호된 문서 요소는 원본 그대로 보존했습니다."),
        codes::DOCX_HEADER_FOOTER_NOT_RENDERED => Warning("머리글과 바닥글은 미리보기에 표시되지 않습니다."),
        codes::DOCX_FLOATING_OBJECT_NOT_RENDERED => Warning("일부 떠 있는 그림은 미리보기에 표시되지 않습니다."),
        codes::DOCX_INLINE_IMAGE_PLACEHOLDER => Warning("문단 안의 그림은 미리보기에서 자리표시자로 표시됩니다."),
        codes::MISSING_VALUE => Warning("값이 비어 있어 일부 차트 요소를 그리지 않았습니다."),
        "HTML_LINK_ACCESSIBLE_NAME_MISSING" => Warning("링크에 읽을 이름이 없습니다. 대체 텍스트나 표시할 이름을 추가하세요."),
        codes::RASTER_IMAGE_PANEL => Warning("이 패널의 그림은 개체 단위로 편집할 수 없습니다."),
        "WEB_EXTERNAL_RESOURCE" => Warning("외부 자원은 내보내기 과정에서 가져오지 않습니다."),
        "WEB_MISSING_RESOURCE" => Warning("참조한 로컬 자원 파일이 없습니다. 결과를 확인해 주세요."),
        "WEB_HTML_UNSCANNED" => Warning("시작 HTML의 자원 참조를 검사하지 못했습니다. 결과를 확인해 주세요."),
        "WEB_INLINED" => Warning("로컬 파일을 HTML 안에 포함했습니다."),
        "WEB_NO_SCENE" => Warning("웹 소스 프로젝트는 장면이 아니라 미리보기로 확인합니다."),
        "WEB_DESIGN_SYSTEM_ID" => Warning("디자인 시스템 연결에 작업물이 지정되어 있지 않습니다."),
        "WEB_DESIGN_SYSTEM_SELF" => Warning("디자인 시스템 연결이 이 작업물 자신을 가리킵니다."),
        "WEB_DESIGN_SYSTEM_PUBLISHER" => Warning("디자인 시스템을 공개하는 작업물은 다른 디자인 시스템을 연결할 수 없습니다."),
        _ => return None,
    })
}

/// The sentence of a refusal's diagnostic code (`diagnostic_words`).
pub(super) fn diagnostic_message(code: &str) -> Option<&'static str> {
    match diagnostic_words(code)? {
        Words::Refusal(sentence) => Some(sentence),
        Words::Warning(_) | Words::Silent => None,
    }
}

pub(super) fn engine_warning_message(diagnostics: &[Diagnostic]) -> Option<String> {
    let mut messages = Vec::new();
    for diagnostic in diagnostics.iter().filter(|diagnostic| !diagnostic.blocking) {
        let message = match diagnostic_words(&diagnostic.code) {
            Some(Words::Warning(sentence)) => sentence,
            // A refusal's code that comes as a note, and a lint finding (its callout words it), add no warning line.
            Some(Words::Refusal(_) | Words::Silent) => continue,
            None if crate::strings::lint_label(&diagnostic.code).is_some() => continue,
            None => "일부 요소를 완전히 표현하지 못했습니다. 결과를 확인해 주세요.",
        };
        if !messages.iter().any(|existing| *existing == message) {
            messages.push(message);
        }
    }
    (!messages.is_empty()).then(|| messages.join(" "))
}

pub(super) fn export_warning(result: &Value) -> Option<String> {
    let diagnostics = result
        .get("diagnostics")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<Diagnostic>>(value).ok())
        .unwrap_or_default();
    engine_warning_message(&diagnostics)
}

pub(super) fn workspace_notice_message(diagnostic: &Diagnostic) -> String {
    match diagnostic.code.as_str() {
        "store_migrated"
        | "store_backup_missing"
        | "store_backup_incomplete_left"
        | "pending_saves_recovered"
        | "store_recovery_warning" => diagnostic.message.clone(),
        _ => "작업공간에서 확인이 필요한 상태가 발견되었습니다. 자세한 내용은 로그를 확인해 주세요.".into(),
    }
}

pub(crate) fn startup_recovery_warning(error: &DltError) -> Option<String> {
    let diagnostic = error.diagnostics.iter().find(|diagnostic| {
        !diagnostic.blocking && diagnostic.code == "core.pending_recovery_before_migration_failure"
    })?;
    let unknown = || "이전 저장의 복구가 일부 진행됐을 수 있습니다. 처리 건수는 확인할 수 없습니다. 자세한 내용은 로그를 확인해 주세요.".to_owned();
    if diagnostic.params.get("counts_known").and_then(Value::as_bool) == Some(true) {
        if let (Some(removed), Some(recovered)) = (
            diagnostic.params.get("removed_pending_temps").and_then(Value::as_u64),
            diagnostic.params.get("recovered_saves").and_then(Value::as_u64),
        ) {
            return Some(format!(
                "이전 저장 복구가 일부 진행된 뒤 시작에 실패했습니다. 임시 파일 {removed}개 정리, 저장 완료 {recovered}건 확인했습니다. 자세한 내용은 로그를 확인해 주세요."
            ));
        }
    }
    Some(unknown())
}

/// Whether a failed request may have been received and carried out by the host: the one judgement for a save
/// (`artifact.save`), a request's draft (`answer_sent_drafts`), the record of an unanswered start (`settle_job_start`) and the
/// edits that repeat or refresh. It goes by the code, not by `retryable`: a refusal (busy, invalid, not found, conflict) changed
/// nothing, even when it may be retried. The host's receipt of a refusal (core `is_recorded`, which holds `RevisionConflict`
/// although it is retryable) only replays that answer for the same id, so a later try with a new id is safe.
pub(super) fn change_result_unknown(error: &DltError) -> bool {
    matches!(&error.code, &ErrorCode::Io | &ErrorCode::Internal | &ErrorCode::HostNotRunning | &ErrorCode::Unavailable | &ErrorCode::StorageFailure)
}

/// The sentence for a web preview window that ended on its own. `stderr` is what the preview process wrote last: one JSON error
/// whose `message` starts with the reason (the PREVIEW_* constants of `web-preview`). A failure without a known reason still says
/// that the window closed; a normal close says nothing.
pub(super) fn preview_exit_message(stderr: &str, success: bool) -> Option<&'static str> {
    let reason = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .and_then(|error| error.get("message").and_then(Value::as_str).map(str::to_owned));
    match reason.as_deref() {
        Some(reason) if reason.starts_with(designllm_web_preview::PREVIEW_UNRESPONSIVE) => {
            Some("미리보기 화면이 응답하지 않아 창을 닫았습니다. 다시 열어 주세요")
        }
        Some(reason) if reason.starts_with(designllm_web_preview::PREVIEW_MEMORY_LIMIT) => {
            Some("미리보기가 메모리 상한을 넘어 창을 닫았습니다. 다시 열어 주세요")
        }
        Some(reason) if reason.starts_with(designllm_web_preview::PREVIEW_PROCESS_EXITED) => {
            Some("미리보기 프로세스가 예기치 않게 끝나 창을 닫았습니다. 다시 열어 주세요")
        }
        _ if success => None,
        _ => Some("미리보기 창이 예기치 않게 닫혔습니다. 다시 열어 주세요"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diagnostic(code: &str, blocking: bool) -> Diagnostic {
        serde_json::from_value(json!({ "code": code, "message": "engine text", "node_id": null, "blocking": blocking, "params": {} }))
            .expect("a diagnostic")
    }

    /// Every engine (flow, slides, canvas, the PPTX export) reports a substituted font as `FONT_SUBSTITUTED`. It reads as
    /// one Korean sentence, not as the generic fallback.
    #[test]
    fn the_font_substitution_code_has_a_korean_sentence() {
        let sentence = "일부 글꼴을 사용할 수 없어 대체 글꼴로 표시했습니다.";
        assert_eq!(designllm_contracts::diag_codes::FONT_SUBSTITUTED, "FONT_SUBSTITUTED");
        assert_eq!(engine_warning_message(&[diagnostic("FONT_SUBSTITUTED", false)]).as_deref(), Some(sentence));
    }

    /// A diagnostic stored before 49f keeps its old lower-case code; it reads as the same sentence as the new code.
    #[test]
    fn an_old_lower_case_diagnostic_reads_as_its_new_code() {
        for (old, new) in [("font_substituted", "FONT_SUBSTITUTED"), ("native_unit_rounding", "NATIVE_UNIT_ROUNDING")] {
            let sentence = engine_warning_message(&[diagnostic(new, false)]).expect("the new code has a warning");
            assert_eq!(engine_warning_message(&[diagnostic(old, false)]), Some(sentence), "{old}");
        }
    }

    /// Every contracts code that an engine reports and the window words (`diag_codes`, `repeat_groups`, the PDF codes)
    /// has its own Korean words in the one table: a refusal's code in the refusal, a warning's code among the warnings.
    /// Neither falls back to the error code's sentence or the generic line.
    #[test]
    fn every_engine_code_the_window_words_has_korean_words() {
        use designllm_contracts::{diag_codes as codes, repeat_groups as repeat};
        const REFUSALS: &[&str] = &[
            codes::PANEL_SIZE_INVALID, codes::PANEL_OUTSIDE_CANVAS, codes::PANEL_OVERLAP, codes::PANEL_TOO_SMALL,
            codes::DATASET_NOT_FOUND, codes::DATASET_REF_STALE, codes::COLUMN_TYPE_INVALID, codes::COLUMN_REQUIRED,
            codes::DATA_AMBIGUOUS, codes::DATA_MALFORMED, codes::NO_PLOTTABLE_DATA, codes::UNIT_REQUIRED,
            codes::NODE_KIND_UNSUPPORTED, codes::NODE_SIZE_INVALID, codes::NODE_OUTSIDE_PANEL, codes::EDGE_ZERO_LENGTH,
            codes::SELF_LOOP_UNSUPPORTED, codes::CONNECTOR_CROSSES_NODE, codes::CONNECTOR_CROSSES_LABEL,
            codes::LABEL_COLLISION, codes::LABEL_OVERFLOW, codes::LABEL_OUTSIDE_PANEL, codes::LEGEND_OVERFLOW,
            codes::COMMON_LEGEND_INVALID, codes::COMMON_LEGEND_OVERFLOW, codes::VISUAL_REVIEW_NOT_APPROVED,
            codes::VISUAL_REVIEW_STALE, codes::EVIDENCE_REVIEW_STATE_INVALID, codes::EVIDENCE_KIND_INVALID,
            codes::EVIDENCE_TEXT_EMPTY, codes::EVIDENCE_HASH_MISMATCH, codes::EVIDENCE_GEOMETRY_INVALID,
            codes::EVIDENCE_REGION_OUTSIDE_CROP, codes::EVIDENCE_PAGE_NOT_IN_SCOPE, codes::GENERATED_IMAGE_CALIBRATION,
            codes::IMAGE_ROTATION_INVALID, codes::CALIBRATION_INVALID, codes::DOCX_DIGITAL_SIGNATURE_UNSUPPORTED,
            codes::DOCX_COMPOUND_CONTAINER_UNSUPPORTED, repeat::CANVAS_REPEAT_GROUP_NOT_EMPTY,
            repeat::CANVAS_REPEAT_CONDITION_SCOPE, repeat::CANVAS_REPEAT_LIMIT, repeat::CANVAS_REPEAT_STALE,
            repeat::CANVAS_REPEAT_FLATTENED, repeat::CANVAS_REPEAT_TARGET, repeat::CANVAS_REPEAT_TABLE,
            repeat::CANVAS_REPEAT_NESTED, repeat::CANVAS_REPEAT_COLUMN, repeat::CANVAS_REPEAT_BINDING,
            repeat::CANVAS_REPEAT_CONDITION, repeat::CANVAS_REPEAT_EXPECTED, designllm_contracts::PDF_MEMORY_LIMIT,
        ];
        const WARNINGS: &[&str] = &[
            codes::FONT_SUBSTITUTED, codes::NATIVE_UNIT_ROUNDING, codes::MISSING_VALUE, codes::RASTER_IMAGE_PANEL,
            codes::SLIDES_THEME_KEPT_AS_SET, codes::DOCX_PROTECTED_INLINE,
            codes::DOCX_PROTECTED_BLOCK, codes::DOCX_HEADER_FOOTER_NOT_RENDERED, codes::DOCX_FLOATING_OBJECT_NOT_RENDERED,
            codes::DOCX_INLINE_IMAGE_PLACEHOLDER, codes::HWPX_PREVIEW_TEXT_REGENERATED, codes::HWPX_UNMODELED_PRESERVED,
            codes::HWPX_FLOATING_OBJECT_NOT_RENDERED, codes::HWPX_LAYOUT_APPROXIMATE,
        ];
        const GENERIC: &str = "일부 요소를 완전히 표현하지 못했습니다. 결과를 확인해 주세요.";
        for code in REFUSALS {
            let sentence = diagnostic_message(code).unwrap_or_else(|| panic!("{code} has no refusal sentence"));
            assert!(has_hangul(sentence) && !sentence.contains(code), "{code}: {sentence}");
            let mut error = DltError::new(ErrorCode::UnsupportedOperation, "refused");
            error.diagnostics.push(diagnostic(code, true));
            assert_eq!(user_error(&error), sentence, "{code}");
            assert_eq!(engine_warning_message(&[diagnostic(code, false)]), None, "{code} as a note adds no warning line");
        }
        for code in WARNINGS {
            assert_eq!(diagnostic_message(code), None, "{code} is a warning, not a refusal");
            let sentence = engine_warning_message(&[diagnostic(code, false)]).unwrap_or_else(|| panic!("{code} shows no warning"));
            assert!(has_hangul(&sentence) && sentence != GENERIC && !sentence.contains(code), "{code}: {sentence}");
        }
        // The shared refusal of a field reads as the error code's sentence (it names no reason of its own).
        let mut error = DltError::new(ErrorCode::UnsupportedOperation, "refused");
        error.diagnostics.push(diagnostic(codes::UNSUPPORTED_FIELD, true));
        assert_eq!(user_error(&error), "이 문서에서는 이 작업을 지원하지 않습니다");
    }

    /// A canvas refusal of a web attribute reaches the window as a Korean sentence, not as the code or the generic line.
    #[test]
    fn the_web_attribute_refusals_reach_the_window_as_korean_sentences() {
        for code in ["WEB_ATTR_FORBIDDEN", "WEB_URL_SCHEME", "WEB_STYLE_SYNTAX", "WEB_ATTR_NO_VALUE"] {
            let sentence = diagnostic_message(code).unwrap_or_else(|| panic!("{code} has no sentence in the table"));
            assert!(sentence.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)), "{code}: {sentence}");
            let mut error = DltError::new(ErrorCode::InvalidArgument, "refused");
            error.diagnostics.push(diagnostic(code, true));
            assert_eq!(user_error(&error), sentence, "{code}");
        }
    }

    /// A PDF refusal that names no page reaches the window with the sentence of its reader code, so a time limit, a stack
    /// limit and a memory limit no longer read as one generic line.
    #[test]
    fn the_pdf_refusal_codes_reach_the_window_as_their_own_sentences() {
        let cases = [
            ("PDF_MEMORY_LIMIT", ErrorCode::UnsupportedOperation),
            ("PDF_STACK_LIMIT", ErrorCode::UnsupportedOperation),
            ("PDF_REQUEST_BUDGET", ErrorCode::UnsupportedOperation),
            ("PDF_INTERNAL_PANIC", ErrorCode::InvalidArgument),
            ("PDF_PARSE_ERROR", ErrorCode::InvalidArgument),
            ("PDF_TOO_LARGE", ErrorCode::InvalidArgument),
        ];
        let count = cases.len();
        let mut sentences = Vec::new();
        for (code, error_code) in cases {
            let sentence = diagnostic_message(code).unwrap_or_else(|| panic!("{code} has no sentence in the table"));
            assert!(has_hangul(sentence), "{code}: {sentence}");
            let mut error = DltError::new(error_code, "the PDF could not be read; the reader's detail is withheld");
            error.diagnostics.push(diagnostic(code, true));
            assert_eq!(user_error(&error), sentence, "{code}");
            sentences.push(sentence);
        }
        sentences.sort_unstable();
        sentences.dedup();
        assert_eq!(sentences.len(), count, "each code has a sentence of its own");
    }

    fn has_hangul(text: &str) -> bool {
        text.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c))
    }

    /// Every web diagnostic code the canvas engine emits (crates/canvas/src/web) reads as a Korean sentence of its own: a
    /// blocking one in the refusal, an informational one among the warnings. Neither falls back to the generic line.
    #[test]
    fn every_web_diagnostic_code_reads_as_a_korean_sentence() {
        const BLOCKING: &[&str] = &[
            "WEB_ATTR_FORBIDDEN", "WEB_URL_SCHEME", "WEB_STYLE_SYNTAX", "WEB_ATTR_NO_VALUE",
            "WEB_TOKEN_ID", "WEB_TOKEN_FONT", "WEB_ENTRY_MISSING", "WEB_ENTRY_PATH", "WEB_ENTRY_TYPE", "WEB_NO_ENTRY",
            "WEB_HEAD_AMBIGUOUS", "WEB_HEAD_MISSING", "WEB_LINK_PRESENT", "WEB_LINK_COUNT", "WEB_TOKEN_DRIFT",
            "WEB_GENERATED_MISSING", "WEB_NAME_CLASH", "WEB_HTML_UNSUPPORTED", "WEB_INLINE_UNSAFE", "WEB_BINARY_RESOURCE",
            "WEB_FORMAT", "WEB_ENCODING", "WEB_BOM", "WEB_DIRECT_FORMATTING", "WEB_PATH", "WEB_TOO_MANY_FILES",
            "WEB_HASH_STALE", "WEB_FILE_TOO_LARGE", "WEB_TOTAL_TOO_LARGE", "WEB_NO_CHANGE", "WEB_PROPERTY",
            "WEB_MAPPING_NOT_FOUND", "WEB_MAPPING_AMBIGUOUS", "WEB_MAPPING_KEY", "WEB_ATTR_MISSING", "WEB_MAP_VOID",
            "WEB_MAP_RAW_TEXT", "WEB_MAP_CHILDREN",
        ];
        const INFORMATIONAL: &[&str] = &[
            "WEB_EXTERNAL_RESOURCE", "WEB_MISSING_RESOURCE", "WEB_HTML_UNSCANNED", "WEB_INLINED", "WEB_NO_SCENE",
            "WEB_DESIGN_SYSTEM_ID", "WEB_DESIGN_SYSTEM_SELF", "WEB_DESIGN_SYSTEM_PUBLISHER",
        ];
        const GENERIC: &str = "일부 요소를 완전히 표현하지 못했습니다. 결과를 확인해 주세요.";
        for code in BLOCKING {
            let sentence = diagnostic_message(code).unwrap_or_else(|| panic!("{code} has no sentence in the table"));
            assert!(has_hangul(sentence) && !sentence.contains(code), "{code}: {sentence}");
            let mut error = DltError::new(ErrorCode::InvalidArgument, "refused");
            error.diagnostics.push(diagnostic(code, true));
            assert_eq!(user_error(&error), sentence, "{code}");
        }
        for code in INFORMATIONAL {
            let sentence = engine_warning_message(&[diagnostic(code, false)]).unwrap_or_else(|| panic!("{code} shows no warning"));
            assert!(has_hangul(&sentence) && sentence != GENERIC && !sentence.contains(code), "{code}: {sentence}");
        }
    }

    /// Every `WEB_*` code in the web engine's source (each `.rs` file of crates/canvas/src/web, read when the test runs, so a new
    /// file or code is checked without editing this test) reads as Korean words of its own. The codes without the prefix that the
    /// engine shares with the canvas engine cannot be told apart from other strings, so they are listed by hand (`SHARED`), and
    /// each must still appear in that source.
    #[test]
    fn every_code_in_the_web_engine_source_has_korean_words() {
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../canvas/src/web");
        let sources: Vec<String> = std::fs::read_dir(&folder)
            .expect("the web engine's source folder")
            .map(|entry| entry.expect("a folder entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .map(|path| std::fs::read_to_string(&path).expect("a source file"))
            .collect();
        assert!(sources.len() >= 5, "the web engine's files are read from {}", folder.display());
        const SHARED: &[&str] = &[
            "ARTIFACT_ID", "DESIGN_SYSTEM_BODY", "EXPORT_FORMAT", "KIND", "KIND_MISMATCH", "OPEN_ASSET_HASH", "PAGE_SELECTION",
            "PROJECT_KIND", "SAVE_FILE", "SAVE_MISMATCH", "SAVE_UNREADABLE", "SIZE", "TEMPLATE",
        ];
        let mut codes: Vec<&str> = sources
            .iter()
            .flat_map(|source| source.match_indices("\"WEB_").map(move |(at, _)| &source[at + 1..]))
            .filter_map(|rest| rest.split_once('"').map(|(code, _)| code))
            .filter(|code| code.len() > 4 && code.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'))
            .collect();
        codes.sort_unstable();
        codes.dedup();
        assert!(codes.len() > 40, "the web engine's codes are found: {codes:?}");
        for code in codes.iter().chain(SHARED) {
            let words = match diagnostic_words(code) {
                Some(Words::Refusal(words) | Words::Warning(words)) => words,
                other => panic!("{code} has no Korean words: {other:?}"),
            };
            assert!(has_hangul(words) && !words.contains(code), "{code}: {words}");
        }
        for code in SHARED {
            assert!(sources.iter().any(|source| source.contains(&format!("\"{code}\""))), "{code} is a web engine code");
        }
    }

    /// The preview window's end: each reason its process writes reads as its own sentence; a failure with no reason reads as the
    /// generic one; a normal close says nothing.
    #[test]
    fn the_preview_exit_reasons_read_as_korean_sentences() {
        let error_line = |message: &str| format!("{{\"code\":\"Unavailable\",\"message\":\"{message}\",\"diagnostics\":[]}}\n");
        let unresponsive = format!("{}; the preview was closed", designllm_web_preview::PREVIEW_UNRESPONSIVE);
        assert_eq!(preview_exit_message(&error_line(&unresponsive), false), Some("미리보기 화면이 응답하지 않아 창을 닫았습니다. 다시 열어 주세요"));
        let memory = format!("{} (2048 MiB for the preview process and its WebView2 processes); the preview was closed", designllm_web_preview::PREVIEW_MEMORY_LIMIT);
        assert_eq!(preview_exit_message(&error_line(&memory), false), Some("미리보기가 메모리 상한을 넘어 창을 닫았습니다. 다시 열어 주세요"));
        let exited = format!("{} (renderer); the preview was closed", designllm_web_preview::PREVIEW_PROCESS_EXITED);
        assert_eq!(preview_exit_message(&error_line(&exited), false), Some("미리보기 프로세스가 예기치 않게 끝나 창을 닫았습니다. 다시 열어 주세요"));
        let generic = Some("미리보기 창이 예기치 않게 닫혔습니다. 다시 열어 주세요");
        assert_eq!(preview_exit_message("", false), generic, "a failure with no reason");
        assert_eq!(preview_exit_message("not an error line", false), generic, "a line that is no error");
        assert_eq!(preview_exit_message("", true), None, "a normal close says nothing");
    }

    /// The refusal of a URL names the addresses it allows, relative ones among them.
    #[test]
    fn the_url_refusal_names_the_addresses_it_allows() {
        let sentence = diagnostic_message("WEB_URL_SCHEME").expect("the refusal has a sentence");
        assert!(sentence.contains("상대 주소"), "{sentence}");
    }
}
pub(super) const AGENT_FOLLOW_NOTICE: &str = "AI가 다른 작업물을 편집했습니다";
pub(super) const AGENT_FOLLOW_BUSY: &str = "편집을 마친 후 작업물을 열어 주세요";

pub(super) fn agent_follow_notice(title: &str) -> String {
    format!("AI가 ‘{title}’ 작업물을 편집했습니다")
}

pub(super) fn agent_editing(revision: u64, changes: usize) -> String {
    format!("AI 편집 중 · 리비전 {revision} · 변경 {changes}개")
}

