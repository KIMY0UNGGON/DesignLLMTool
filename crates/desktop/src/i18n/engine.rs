//! English of the few engine texts (designllm-contracts) the window shows as they are. Kept by hand: the generated area
//! tables come from the desktop's own literals. Engine diagnostics in general stay Korean (out of scope).

pub(super) const ENTRIES: &[(&str, &str)] = &[
    (
        "사용자 선택 출력 프로필입니다. 특정 학회·저널의 최신 공식 규정을 보장하지 않습니다.",
        "This is an output profile you chose. It does not guarantee the latest official rules of any conference or journal.",
    ),
    // figure_rules::FIGURE_OUTPUT_PROFILES labels: the home screen's size menu for a figure shows them.
    ("단일 단 (85 × 65 mm)", "Single column (85 × 65 mm)"),
    ("1.5단 (120 × 90 mm)", "1.5 column (120 × 90 mm)"),
    ("2단 (175 × 120 mm)", "Double column (175 × 120 mm)"),
    ("고해상도 인쇄 (175 × 120 mm · 600 DPI)", "High-resolution print (175 × 120 mm · 600 DPI)"),
    ("발표 16:9 (254 × 143 mm · 150 DPI)", "Presentation 16:9 (254 × 143 mm · 150 DPI)"),
];
