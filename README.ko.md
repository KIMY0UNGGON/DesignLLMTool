# DesignLLMTool

[English](README.md)

DesignLLMTool은 MCP 서버가 내장된 Windows, macOS, Linux용 데스크톱 디자인 앱입니다. 사용자가 앱 창에서 지켜보는 동안 AI 에이전트(`Claude Code`, `Codex CLI`, `Gemini CLI`, `AGY`, `Claude Desktop`, `Cursor`, `VS Code` 및 기타 MCP 클라이언트)가 편집 가능한 디자인 아티팩트를 생성하고 수정합니다.

## 목차

- [주요 기능](#주요-기능)
- [요구 사항](#요구-사항)
- [빠른 시작](#빠른-시작)
- [설치 프로그램 및 클라이언트 설정](#설치-프로그램-및-클라이언트-설정)
- [서버 옵션](#서버-옵션)
- [데스크톱 앱](#데스크톱-앱)
- [소스에서 빌드](#소스에서-빌드)
- [라이선스](#라이선스)

## 주요 기능

| 아티팩트 종류 | 용도 | 저장 형식 |
| --- | --- | --- |
| `Canvas` | 화면, 랜딩 페이지, 포스터, 소셜 게시물, 논문 아키텍처 다이어그램 | 네이티브 파일 |
| `Slides` | 프레젠테이션 덱 | `PPTX` |
| `Figure` | PDF 근거 자료를 포함한 CSV 기반 데이터 플롯 | 네이티브 파일 |
| `DOCX`, `HWPX`, `HWP` | 문서 | 고유 형식 |
| `Web code` | 웹 프로젝트 | 네이티브 파일 |

내보내기 형식: `PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, `ZIP`(웹 코드). 엔진이 지원하지 않는 형식은 거부됩니다.

### 디자인 가이드

- `design_guide`는 내장 가이드를 반환하며, `glossary` 토픽은 가이드에서 사용하는 모든 디자인 용어를 정의합니다.
- 연결이 아티팩트의 가이드를 받기 전에 전송된 변경 요청(`changes_validate` 포함)은 해당 가이드와 함께 `GUIDE_REQUIRED`로 거부됩니다.
- `artifact_create`, `artifact_open`, `artifact_inspect`, `context_read`, `artifact_validate`의 결과에도 해당 가이드가 포함됩니다.
- 가이드를 한 번 전달받으면 해당 연결에서 같은 가이드를 사용하는 모든 아티팩트(예: 모든 덱)에 적용됩니다. 토픽만 지정하여 `design_guide`를 호출하는 것은 참고용 열람이며 잠금을 해제하지 않습니다.
- 덱이나 v1.2 값을 사용하는 `Canvas`에 변경 사항을 적용하면 디자인 품질 경고가 반환되며, 공유 토큰 변경 후 다른 아티팩트에서 경고가 발생하면 해당 아티팩트의 이름이 표시됩니다.

### 논문 다이어그램

- `paper-diagram` 가이드를 따릅니다. 그림 내부에 제목 없이 인쇄 크기의 `Canvas`를 사용하고, 유형별 도형(사다리꼴 인코더, 원통, `circle_plus` 잔차 연결), 직교 화살표, `N×` 반복 프레임을 사용합니다.
- `design_guide`의 `paper-diagram-example` 토픽은 한 번의 `changes_apply`로 실행할 수 있는 연산 목록 형태의 완성된 Transformer encoder 그림을 반환합니다.

### 논문 그림

- `paper-figure` 가이드를 따릅니다. `Figure` 아티팩트, Okabe-Ito 색상, 단위가 포함된 축 레이블을 사용합니다.
- `Figure` 내부의 다이어그램은 사각형, 타원, 수식, 직선 연결선만 지원합니다. 꺾은선 연결선과 유형별 도형은 `Canvas`에서 그립니다.
- `figure.output_profile` 명령은 출력 프로필(단일 단, 1.5단, 2단, 고해상도 인쇄, 발표 16:9)을 설정합니다.

<details>
<summary>Figure 출력 프로필 색상 규칙</summary>

- `figure.output_profile`은 모든 플롯 패널에 팔레트의 첫 번째 색상을 지정하여 동일한 계열이 패널 간에 일치하도록 합니다.
- 이후 계열은 팔레트 순서(Okabe-Ito이며, 고해상도 인쇄와 발표 프로필은 검정에 가까운 색으로 시작)를 따르며, 대응 관계가 명확한 공통 범례 견본은 해당 계열과 일치하도록 색상이 다시 지정됩니다.
- 프로필을 선택하지 않은 `Figure`는 기존 색상을 유지합니다. 다만 계열이 2개 이상인 플롯에서 패널 색상이 출력 프로필 팔레트에 포함되거나 이후 계열이 패널 색상을 반복한 경우는 예외입니다. 이런 플롯은 현재 규칙의 계열 색상을 적용받으며 시각적 승인을 다시 받아야 합니다.

</details>

### 디자인 품질 규칙

- 레이아웃으로 구조를 설명하며 보조 레이블은 생략합니다. 요청이 없는 한 카드나 상자의 어느 한쪽 가장자리(위, 아래, 왼쪽, 오른쪽)에 있는 색상 막대는 `ACCENT_BAR`로 보고됩니다.
- 기본 프레임워크 및 어시스턴트 색상(Tailwind 파랑/남색/에메랄드/빨강, Bootstrap, Material, Google 파랑, ChatGPT 초록, Claude 주황, Gemini 보라, 짙은 네이비 배경면)은 `COLOR_STOCK_DEFAULT`로 보고됩니다. 내장 `Canvas` 템플릿은 기본 프레임워크 색상을 사용하지 않으며, 내장 랜딩 페이지 템플릿의 기능 카드에는 외곽선 아래 그림자가 없습니다.
- `ICON_EMOJI`는 아이콘 대신 사용된 체크, X, 별, 반짝임, 화살표 기호(`✓ ✗ ★ ✦ →`) 뒤에 공백과 텍스트가 이어지는 경우도 보고합니다.
- 추가 비차단 경고: `SHADOW_HARD_OFFSET`(흐림 없이 2 px 이상 떨어져 있고 불투명도가 0.25 이상인 그림자, 즉 네오브루탈리스트 블록 그림자), `SHADOW_GLOW`(오프셋 없이 색상이 있는 그림자, 즉 색상 후광), `CARD_GHOST`(흐림이 16 px 이상인 그림자 아래에 2 px 이하의 외곽선이 있는 경우; 입체감은 한 번만 표현하며 새 도형은 기본적으로 1 pt 회색 외곽선을 가지므로 그림자를 유지하는 카드에서는 `stroke_width`를 0으로 설정), `CARD_NESTED`(더 큰 그림자 카드 안에 들어 있는 그림자 카드), `TEXT_EYEBROW`(제목 바로 위의 짧은 대문자 또는 자간이 넓은 레이블), `TEXT_SECTION_NUMBER`(한 페이지에 장식용 `01`, `02`, `03`을 배치한 경우).
- 내장 디자인 가이드는 요청이 허용하는 범위에서 큰 숫자 히어로 지표, 코드나 데이터가 아닌 텍스트에 장식으로 쓰는 고정폭 글꼴, 디스플레이 서체로의 Impact 또는 Arial Black, 뒤에 주제가 없는 줄무늬나 격자 질감, 지어낸 숫자를 피하고, 입체감은 한 번만 표현하며, 작업이 쓰이는 환경에 맞춰 밝은 페이지 또는 어두운 페이지를 선택하고, 이러한 기본값보다 사용자 요청이 우선한다고 안내합니다.
- 이 규칙들은 오픈소스 디자인 스킬 [impeccable](https://github.com/pbakaus/impeccable)(Apache-2.0)의 안티패턴을 참고해 고쳐 쓴 것입니다.
- 덱은 항상 검사합니다. `Canvas`는 v1.2 값(모서리 반경, 불투명도, 그림자, 그라데이션 또는 경로)을 사용할 때만 `ACCENT_BAR` 및 `COLOR_STOCK_DEFAULT`를 포함한 모든 비차단 디자인 품질 경고를 받으므로 기존 `Canvas`의 검증 출력은 변경되지 않습니다.

### 다이어그램 도형

- 편집기 도구 상자(왼쪽 패널, `도구` / `Tools` 탭)의 `다이어그램 도형` / `Diagram shapes`에서 인코더 및 디코더(사다리꼴), 데이터(평행사변형), 저장소(원통), 더하기(`+`가 표시된 원), 곱하기(`×`가 표시된 원), 분기(마름모), 고정 모듈(육각형), 텐서(직육면체)를 제공합니다.
- 에이전트는 `designllm://shapes` 또는 `design_guide`의 `shapes-json` 토픽을 참조하여 이 도형들을 `path` 노드(`changes_apply`, `add_node`)로 추가하며, `add.shape` GUI 명령은 사용자나 `ui_command_run`을 위해 동일한 작업을 수행합니다.

## 요구 사항

- **Windows**:
  - Windows 10 또는 11, 64비트.
  - 저장소를 복제하기 위한 Git(바로 실행할 수 있는 빌드가 저장소에 포함되어 있으며 체크아웃 기준 약 115 MB입니다).
  - 선택 사항(Windows 전용): Microsoft Word(PDF를 편집 가능한 DOCX 사본으로 여는 경우에만 필요).
  - 선택 사항(Windows 전용): Microsoft Edge WebView2 Runtime(Windows 11에 기본 포함, 웹 코드 미리 보기 및 자동 비디오 포스터 추출용).
- **macOS 및 Linux**:
  - macOS 11 이상(Apple silicon 및 Intel) 또는 X11이나 Wayland 데스크톱 환경의 Linux(`x64` 및 `arm64`).
  - 저장소를 복제하기 위한 Git(macOS 및 Linux용 패키지 바이너리는 저장소에 포함되어 있지 않으며 `install.sh`가 릴리스 패키지를 내려받습니다), 그리고 `curl` 또는 `wget`, `tar`, `sha256sum` 또는 `shasum`(macOS와 데스크톱 Linux 배포판에는 모두 기본 포함되어 있습니다).
  - 소스 빌드(`--build` 옵션을 사용하거나 다운로드에 실패한 경우)에만 필요한 도구: Rust 툴체인(`1.93.1`, [rustup.rs](https://rustup.rs)에서 설치)과 C 컴파일러(SQLite를 소스에서 컴파일하므로 macOS에서는 Xcode Command Line Tools(`xcode-select --install`), Linux에서는 `build-essential` 또는 이에 준하는 패키지).
  - Linux 런타임 요구 사항: 내려받은 실행 파일은 glibc 2.35 이상(Ubuntu 22.04 이상, Debian 12 이상, Fedora 36 이상)이 필요합니다(구형 시스템에서는 내려받은 실행 파일이 시작되지 않으므로 `./install.sh --build`를 사용합니다). 또한 OpenGL 또는 Vulkan 드라이버가 포함된 X11이나 Wayland 세션, 파일 열기/저장 대화 상자를 위한 `zenity` 또는 `kdialog`, 그리고 데스크톱 창이 런타임에 로드하는 키보드 라이브러리 `libxkbcommon-x11`(Wayland에서는 `libxkbcommon`)이 필요하며, 없으면 창이 열리지 않습니다. Ubuntu, Fedora 등의 데스크톱 설치에는 포함되어 있고 최소 설치에서는 배포판 패키지(Debian 및 Ubuntu의 경우 `libxkbcommon-x11-0`)로 추가할 수 있으며, 창에만 필요하므로 `--mcp --background`는 이 라이브러리 없이도 실행됩니다.

## 빠른 시작

1. 저장소를 복제합니다:

   ```powershell
   git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git
   cd DesignLLMTool
   ```

2. 설치 프로그램을 실행합니다:

   Windows(`install.ps1`):

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```

   `install.ps1`은 `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe`를 사용하여 정상 실행 여부를 확인한 뒤, `Claude Code`, `Codex CLI`, `AGY` 중 설치되어 있는 모든 클라이언트에 `designllm` MCP 서버를 등록합니다. 요청된 등록에 실패하면 종료 코드 `1`로 종료합니다.

   macOS 및 Linux(`install.sh`):

   ```bash
   ./install.sh
   ```

   `install.sh`는 Rust 없이 동작합니다. 복제한 폴더에 `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool`(`<os>`는 `macos` 또는 `linux`, `<arch>`는 `x64` 또는 `arm64`)이 없으면 GitHub 릴리스 [`v0.1.2`](https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2)에서 해당 플랫폼의 `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz`를 `curl` 또는 `wget`으로 내려받고 `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256` 파일로 검사한 뒤, 패키지 폴더 바깥의 파일이 포함된 아카이브는 거부하고 `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`에 압축을 풉니다(내려받은 폴더는 git에서 무시되며 이후 실행 시에는 다시 내려받지 않고 재사용합니다. `DESIGNLLM_RELEASE_URL`로 다운로드 폴더 URL을 바꿀 수 있으며 기본값은 `https://github.com/KIMY0UNGGON/DesignLLMTool/releases/download/v<version>`이고 `<version>`은 `Cargo.toml`의 버전입니다). 다운로드에 실패한 경우(네트워크 없음, 해당 버전의 릴리스 없음, 체크섬 불일치) 또는 `--build`를 지정한 경우에만 이전처럼 Rust `1.93.1`(`rustup`이 있으면 이를 사용하고, 없으면 `PATH`의 `cargo` 사용)로 소스에서 빌드하여 `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`에 생성합니다(`--client print`에서는 다운로드 실패 시 빌드하는 대신 빌드 안내를 출력합니다). 정상 실행 여부를 확인한 뒤 `Claude Code`, `Codex CLI`, `AGY` 중 설치되어 있는 모든 클라이언트에 `designllm` MCP 서버를 등록하며, 요청된 등록에 실패하면 종료 코드 `1`로 종료합니다. `DISPLAY`나 `WAYLAND_DISPLAY`가 없는 Linux 환경에서는 `--background`를 사용하라는 경고를 표시합니다.

3. AI 클라이언트를 다시 시작하고 `design_guide` 도구를 호출하도록 요청합니다.

> [!TIP]
> `design_guide`가 디자인 가이드 색인을 반환하면 연결이 정상 작동하는 것입니다.

## 설치 프로그램 및 클라이언트 설정

> [!IMPORTANT]
> 기존 `designllm` 항목이 이 복제본의 실행 파일(`release/DesignLLMTool-*-mcp-*/` 또는 `target/package/DesignLLMTool-*-mcp-*/`, 예: 이전 버전)을 가리키는 경우, 설치 프로그램은 이를 현재 실행 파일로 교체하고 업데이트됨으로 보고합니다(단, 인수가 이번 실행 옵션과 다르거나 `Claude Code`에서 사용자 범위가 아닌 경우는 제외). 다른 위치(다른 복제본, 다른 프로그램)를 가리키는 항목은 변경하지 않으므로, 해당 항목이 정의된 클라이언트 설정에서 직접 변경하거나 제거한 뒤 `install.ps1`(또는 `install.sh`)을 다시 실행합니다.

> [!NOTE]
> `Gemini CLI`의 경우 `install.ps1`과 `install.sh`는 서버를 등록하는 대신 `mcpServers` 항목을 출력합니다. 확장에서 `designllm`을 제공할 수 있으므로 먼저 `gemini mcp list`를 확인합니다.

### 설치 프로그램 옵션

| `install.ps1` | `install.sh` | 동작 |
| --- | --- | --- |
| `-Project "<existing folder>"` | `--project DIR` | 작업 공간을 해당 폴더에 유지합니다. |
| `-Client claude\|codex\|gemini\|agy\|print` | `--client auto\|claude\|codex\|gemini\|agy\|print` | 클라이언트 하나만 등록하거나 설정만 출력합니다. |
| `-Background` | `--background` | 창을 열지 않고 도구를 실행합니다. |
| `-DryRun` | `--dry-run` | 아무것도 실행하거나 변경하지 않고 명령어만 출력합니다(`--dry-run`은 아무것도 내려받지 않습니다). |
| — | `--build` | 내려받거나 `release/` 아래의 패키지 바이너리를 재사용하는 대신 소스에서 `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`로 빌드합니다. |

### 클라이언트 설정 개요

| 클라이언트 | 방식 | 설정 형식 / 대상 |
| --- | --- | --- |
| `Claude Code`, `Codex CLI`, `AGY` | `install.ps1` / `install.sh`로 등록 | 클라이언트 CLI MCP 레지스트리 |
| `Gemini CLI` | `install.ps1` / `install.sh`로 출력 | Windows에서는 `%USERPROFILE%\.gemini\settings.json`(또는 `GEMINI_CLI_HOME` 아래의 `.gemini\settings.json`), macOS 및 Linux에서는 `~/.gemini/settings.json`의 `mcpServers` JSON |
| `Claude Desktop`, `Cursor` | `install.ps1` / `install.sh`로 출력 | `mcpServers` JSON |
| `VS Code` | `install.ps1` / `install.sh`로 출력 | `.vscode/mcp.json`의 `servers` JSON |

<details>
<summary>설치 프로그램 동작 세부 정보</summary>

- **기존 등록 항목**: 기존 `designllm` 항목의 명령어가 이 복제본의 실행 파일(`release/DesignLLMTool-*-mcp-*/` 또는 `target/package/DesignLLMTool-*-mcp-*/`, 예: 이전 버전)인 경우, 현재 실행 파일로 교체하고 업데이트됨으로 보고합니다. 이때 교체된 항목에 수동으로 설정한 환경 변수는 이어지지 않습니다. 인수가 이번 실행 옵션과 다르거나 `Claude Code`에서 사용자 범위가 아닌 항목은 메시지와 함께 그대로 유지합니다. 다른 위치(다른 복제본, 다른 프로그램)를 가리키는 항목은 복원할 수 없는 설정이 들어 있거나 프로젝트 설정에서 비롯된 항목일 수 있으므로 변경하지 않으며, 이 경우 해당 항목이 정의된 클라이언트 설정에서 변경하거나 제거한 뒤 `install.ps1`(또는 `install.sh`)을 다시 실행합니다.
- **검증 및 읽을 수 없는 클라이언트**: 각 등록 후 설치 프로그램은 해당 항목을 다시 읽어 이 사본의 `DesignLLMTool.exe`(macOS 및 Linux에서는 `DesignLLMTool`)를 실행하는 경우에만 등록된 것으로 간주합니다. 클라이언트가 결과를 명확하게 보고하지 않거나 서버 목록을 읽을 수 없는 경우 해당 클라이언트를 변경하지 않고 미등록 상태로 보고합니다.
- **`Gemini CLI`**: `gemini mcp add`는 다른 서버 설정을 포함한 전체 Gemini 설정 파일을 다시 쓰며 저장하지 않고도 성공을 보고할 수 있으므로 설치 프로그램은 이를 건너뜁니다. 대신 `mcpServers` 항목을 출력하며, 확장에서 `designllm`을 제공할 수 있으므로 먼저 `gemini mcp list`를 확인합니다.
- **배치 심(Batch shim)**: Windows에서 클라이언트가 `.cmd` 또는 `.bat` 심을 통해 실행되고 경로에 `cmd.exe`가 해석하는 문자가 포함된 경우, 설치 프로그램은 해당 클라이언트를 등록하지 않고 미등록 상태로 보고한 뒤 설정을 출력합니다. `Codex CLI`의 경우 `[mcp_servers.designllm]` TOML 항목과 Codex 설정 경로(`%USERPROFILE%\.codex\config.toml` 또는 `CODEX_HOME`의 `config.toml`)도 함께 출력합니다.
- **출력되는 설정**: `Gemini CLI`, `Claude Desktop`, `Cursor`, `VS Code`의 경우 설치 프로그램은 붙여넣을 JSON 항목을 출력합니다(앞의 세 클라이언트는 `mcpServers`, `VS Code`는 `servers`).
- **macOS 공증**: macOS 실행 파일은 공증되지 않았습니다. `install.sh`는 `curl` 또는 `wget`으로 내려받으므로 격리 플래그가 설정되지 않아 바로 실행됩니다. 웹 브라우저로 아카이브를 대신 내려받은 경우 `xattr -d com.apple.quarantine DesignLLMTool`로 플래그를 제거해야 macOS가 실행을 허용합니다.

</details>

<details>
<summary>클라이언트별 수동 등록 명령어</summary>

`<repo>`를 복제한 폴더 경로로 바꿉니다.

**Windows**:

Claude Code:

```powershell
claude mcp add --scope user designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

Codex CLI(PowerShell에서 `codex.ps1`을 실행하면 단독 `--`가 누락되므로 `codex.cmd`를 입력합니다):

```powershell
codex.cmd mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

AGY:

```powershell
agy mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

`Gemini CLI`(`%USERPROFILE%\.gemini\settings.json`), `Claude Desktop`, `Cursor`(`mcpServers` JSON):

```json
{
  "mcpServers": {
    "designllm": {
      "command": "<repo>\\release\\DesignLLMTool-0.1.2-mcp-win-x64\\DesignLLMTool.exe",
      "args": ["--mcp"]
    }
  }
}
```

VS Code(`.vscode/mcp.json`):

```json
{
  "servers": {
    "designllm": {
      "type": "stdio",
      "command": "<repo>\\release\\DesignLLMTool-0.1.2-mcp-win-x64\\DesignLLMTool.exe",
      "args": ["--mcp"]
    }
  }
}
```

**macOS 및 Linux**(`<os>`는 `macos` 또는 `linux`, `<arch>`는 `x64` 또는 `arm64`):

아래 경로는 `install.sh`가 내려받아 푸는 폴더인 `<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool`을 가리킵니다. 소스 빌드는 `<repo>/target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool`에 생성됩니다.

`install.sh` 없이 수동으로 설치하려면 GitHub 릴리스 [`v0.1.2`](https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2)에서 `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz`와 `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256`을 내려받아 체크섬을 검증하고(`shasum -a 256 -c DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256`), 압축을 푼 뒤(`tar -xzf DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz`) `<folder>/DesignLLMTool --mcp`를 클라이언트에 등록합니다. 아카이브에는 `DesignLLMTool-0.1.2-mcp-<os>-<arch>/` 폴더 안에 `DesignLLMTool`, `assets/fonts`, `THIRD_PARTY_LICENSES.txt`, `THIRD_PARTY_NOTICES.txt`가 들어 있으며, 실행 파일은 같은 위치의 `assets/fonts`가 필요하므로 압축을 푼 폴더 구성을 그대로 유지해야 합니다. macOS에서 웹 브라우저로 내려받은 경우에는 실행 전에 `xattr -d com.apple.quarantine DesignLLMTool`을 실행합니다.

Claude Code, Codex CLI(`codex.cmd` 불필요), AGY:

```bash
claude mcp add --scope user designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
codex mcp add designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
agy mcp add designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
```

`Gemini CLI`(`~/.gemini/settings.json`), `Claude Desktop`, `Cursor`(`mcpServers` JSON):

```json
{
  "mcpServers": {
    "designllm": {
      "command": "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool",
      "args": ["--mcp"]
    }
  }
}
```

VS Code(`.vscode/mcp.json`):

```json
{
  "servers": {
    "designllm": {
      "type": "stdio",
      "command": "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool",
      "args": ["--mcp"]
    }
  }
}
```

</details>

### 업데이트

복제한 폴더에서 `git pull`을 실행한 다음 처음 설치할 때 사용한 것과 같은 옵션(예: 같은 `-Project` / `--project` 및 `-Background` / `--background`)으로 설치 프로그램(Windows에서는 `install.ps1`, macOS 및 Linux에서는 `./install.sh`)을 다시 실행하고, AI 클라이언트를 다시 시작합니다.

## 서버 옵션

| 플래그 | 동작 |
| --- | --- |
| `--mcp` | stdio를 통해 MCP 서버로 실행합니다. 첫 번째 도구 호출 시 데스크톱 창이 열리며, 창 없이 실행하려면 호출 시 `background: true`를 전달하거나 `--background`로 시작합니다. |
| `--project "<existing folder>"` | 작업 공간을 `<folder>\.designllm`에 두고 네이티브 파일은 `<folder>\artifacts`에 저장합니다. 이 플래그가 없으면 기본 작업 공간은 Windows에서 `%LOCALAPPDATA%\DesignLLMTool\workspaces\default`, macOS에서 `~/Library/Application Support/DesignLLMTool/workspaces/default`, Linux에서 `$XDG_DATA_HOME/DesignLLMTool/workspaces/default`(기본값 `~/.local/share/DesignLLMTool/workspaces/default`)가 되며, 모든 플랫폼에서 `DESIGNLLM_WORKSPACE`로 재정의할 수 있습니다. |

- **업데이트 확인(`--mcp`)**: `--mcp`로 실행하면 백그라운드 확인이 GitHub 릴리스 API에 최신 릴리스를 최대 24시간마다 한 번 요청하고, 결과를 사용자별 앱 데이터 폴더(`DesignLLMTool/update-check.json`: Windows에서는 `%LOCALAPPDATA%`, macOS에서는 `~/Library/Application Support`, Linux에서는 `$XDG_DATA_HOME` 또는 `~/.local/share`)에 캐시합니다. 이 확인은 MCP 핸드셰이크나 도구 호출을 지연시키지 않으며, 요청이 실패해도 조용히 넘어갑니다. 더 새로운 릴리스가 있으면 연결당 한 번 첫 번째 성공 도구 결과에 두 버전과 업데이트 방법을 알리는 안내와 함께 `structuredContent.update_available`(`latest_version`, `current_version`, `url`, `how_to_update`)이 포함됩니다. 끄려면 `DESIGNLLM_NO_UPDATE_CHECK=1`을 설정합니다.

### 플랫폼 차이

- **Windows 전용 기능**(macOS 및 Linux에서는 지원되지 않음):
  - 웹 코드 미리 보기 창(WebView2 필요). 대신 웹 코드를 `HTML` 또는 `ZIP`으로 내보내 브라우저에서 엽니다.
  - 자동 비디오 포스터 추출(WebView2 필요).
  - PDF를 편집 가능한 `DOCX` 사본으로 열기(Microsoft Word COM 자동화 필요).
- **그 외 모든 기능**: 아티팩트 종류, 편집, 렌더링, 내보내기(`PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, `ZIP`, `DOCX`, `HWPX`, `HWP`)는 모든 플랫폼에서 동일하게 작동합니다.
- **호스트 연결**: Windows에서는 명명된 파이프를 사용합니다. macOS 및 Linux에서는 해당 사용자만 열 수 있는 디렉터리(`$XDG_RUNTIME_DIR` 또는 권한 모드 `0700`의 `<temp>/dllm-<uid>`)의 Unix 도메인 소켓을 사용하므로 모든 클라이언트를 같은 사용자로 실행해야 합니다.
- **AI 작업 프로세스**: Windows에서는 Windows 작업 객체를 사용하고, macOS 및 Linux에서는 작업별 프로세스 그룹을 사용하여 `SIGKILL`로 함께 종료합니다.
- **그래픽**: OpenGL을 우선 사용하고 기본 백엔드에서 `wgpu`로 폴백합니다(Windows는 DirectX 12, macOS는 Metal, Linux는 Vulkan). `DESIGNLLM_RENDERER=glow|wgpu|wgpu-warp`로 특정 렌더러를 지정할 수 있습니다.

## 데스크톱 앱

AI 없이 앱을 사용하려면 앱을 직접 실행하거나 프로젝트 폴더를 전달합니다:

Windows:

```powershell
.\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe
.\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe --project "<existing folder>"
```

macOS 및 Linux(소스 빌드의 경우 `release` 대신 `target/package`를 사용합니다):

```bash
./release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool
./release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool --project "<existing folder>"
```

- **인터페이스 언어 전환**: 홈 화면에서 작업 공간 메뉴(왼쪽 위 작업 공간 이름)를 열고 `한국어` 또는 `English`를 선택합니다. 설정은 즉시 적용되며 작업 공간의 `.gui\settings.json`에 저장됩니다.
- **다이어그램 도형 추가**: `Canvas`에서 편집기 도구 상자(왼쪽 패널, `도구` / `Tools` 탭)를 열고 `다이어그램 도형` / `Diagram shapes`를 찾아 타일을 클릭하거나 페이지로 드래그합니다.

## 소스에서 빌드

```powershell
rustup toolchain install 1.93.1
cargo +1.93.1 build --release -p designllm-application
cargo +1.93.1 test -p designllm-bridge -p designllm-contracts -p designllm-canvas -p designllm-runtimes
```

릴리스 빌드는 Windows에서 `target\release\DesignLLMTool.exe`를 생성하며(패키지된 빌드를 업데이트하려면 `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe`에 덮어씁니다), macOS 및 Linux에서는 `target/release/DesignLLMTool`을 생성합니다. 이 실행 파일은 같은 디렉터리에 `assets/fonts`가 필요합니다(`install.sh --build` 또는 다운로드 실패 시 `install.sh`가 `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/` 구성을 생성합니다).

GitHub Actions 워크플로(`.github/workflows/build.yml`)는 Windows, macOS(`arm64` 및 `x64`), Linux(`x64` 및 `arm64`)에서 빌드와 테스트를 수행하고, 모든 빌드마다 각 `DesignLLMTool-<version>-mcp-<platform>.tar.gz` 패키지 옆에 `.sha256` 파일을 작성하고 두 파일을 모두 워크플로 아티팩트로 업로드하며, `v*` 태그를 푸시할 때는 두 파일을 GitHub 릴리스에 첨부합니다. `.github/workflows/install-check.yml` 워크플로(수동 실행 또는 릴리스 게시 시 실행)는 macOS(`arm64`, `x64`)와 Linux(`x64`, `arm64`)의 새 클론에서 `PATH`로부터 `cargo`와 `rustup`을 제거한 상태로 `install.sh`가 릴리스 패키지의 압축을 풀고 빌드는 수행하지 않았는지 확인한 뒤 `DESIGNLLM_NO_UPDATE_CHECK=1`로 MCP 세션을 실행합니다.

## 라이선스

서드파티 라이선스와 고지 사항은 `release\DesignLLMTool-0.1.2-mcp-win-x64\THIRD_PARTY_LICENSES.txt` 및 `release\DesignLLMTool-0.1.2-mcp-win-x64\THIRD_PARTY_NOTICES.txt`에 있습니다. Pretendard 글꼴은 SIL Open Font License(`assets\fonts\OFL-Pretendard.txt`)를 따릅니다.
