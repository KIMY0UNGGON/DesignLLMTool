# DesignLLMTool

[English](README.md)

DesignLLMTool은 MCP 서버가 내장된 Windows 데스크톱 디자인 앱입니다. AI 에이전트(`Claude Code`, `Codex CLI`, `Gemini CLI`, `AGY`, `Claude Desktop`, `Cursor`, `VS Code` 및 기타 MCP 클라이언트)는 이를 사용해 사용자가 앱 창에서 지켜보는 동안 편집 가능한 디자인 작업물을 생성하고 수정합니다.

## 주요 기능

- **작업물 종류**: `Canvas`(화면, 랜딩 페이지, 포스터, 소셜 포스트, 논문 아키텍처 다이어그램), `Slides`(PPTX로 저장), `Figure`(PDF의 근거를 포함한 CSV 데이터 플롯), `DOCX`, `HWPX`, `HWP` 문서 및 `Web code`.
- **내보내기 및 저장**: `PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, `ZIP`(웹 코드)으로 내보내며, 문서는 각 원본 형식으로 저장합니다.
- **필수 디자인 가이드**: `design_guide` 도구는 내장 가이드를 반환합니다. 연결이 특정 작업물의 가이드를 받기 전에 에이전트가 해당 작업물에 변경을 보내면 그 첫 번째 변경은 `GUIDE_REQUIRED`로 거부되며, 거부 응답에 해당 가이드가 함께 전달됩니다. 한 번 전달된 가이드는 같은 연결에서 같은 가이드를 사용하는 다른 모든 작업물(예: 모든 슬라이드 덱)에도 적용됩니다. 토픽만 지정해 `design_guide`를 호출하는 것은 참고용 열람이며 아무것도 잠금 해제하지 않습니다. `Canvas` 또는 슬라이드 덱에 변경을 적용할 때마다 결과에 해당 작업물의 디자인 품질 경고가 나열되며, 공유 토큰 변경 후 다른 작업물에 속한 경고가 발생하면 결과에 해당 작업물의 이름이 표시됩니다. 디자인 용어집(`design_guide` 토픽 `glossary`)은 가이드에서 사용하는 모든 디자인 용어를 정의합니다.
- **논문 아키텍처 다이어그램 및 데이터 플롯**: 아키텍처, 파이프라인, 방법론 개요 그림은 `paper-diagram` 가이드를 따릅니다. 인쇄 크기의 `Canvas`를 사용하고, 그림 내부에 제목을 넣지 않으며, 타입 지정 도형(사다리꼴 인코더, 실린더, `circle_plus` 잔차 연결 등), 직교 화살표, `N×` 반복 프레임을 사용합니다. `design_guide`의 `paper-diagram-example` 토픽은 완성된 Transformer encoder 그림을 단일 `changes_apply`로 반환합니다. 데이터 플롯은 `paper-figure` 가이드를 따릅니다. `Figure` 작업물, Okabe-Ito 색상, 단위가 포함된 축 라벨을 사용합니다. `Figure` 내부의 다이어그램은 사각형, 타원, 수식, 직선 연결선만 지원하므로, 꺾인 화살표와 타입 지정 도형은 `Canvas`에 그립니다. Figure 출력 프로필(single column, double column, print, presentation)은 모든 플롯 패널에 해당 팔레트의 첫 번째 색을 지정하므로, 동일한 계열은 모든 패널에서 같은 색을 갖습니다. 이후 계열은 팔레트 순서를 따르며(Okabe-Ito 기준이며, print와 presentation은 검정에 가까운 색부터 시작함), 공통 범례의 색 견본도 해당 계열에 맞게 색이 다시 지정됩니다. 프로필을 선택하지 않은 `Figure`는 기존 색을 유지합니다. 다만 계열이 둘 이상인 플롯에서 패널 색이 출력 프로필 팔레트에 속하거나 이후 계열이 패널 색을 반복한 경우는 예외이며, 이 경우 해당 플롯에는 현재 규칙의 계열 색이 지정되고 시각 승인을 다시 받아야 합니다.

- **일반화된 AI 스타일 방지 규칙**: 레이아웃으로 구조를 설명하며 보조 라벨은 생략합니다. 카드나 박스의 상단 또는 왼쪽 가장자리에 색 띠를 두르는 형태(블로그 콜아웃 스타일)는 요청이 없는 한 피하며 `ACCENT_BAR`로 보고됩니다. 프레임워크 및 어시스턴트 기본 색상(Tailwind blue/indigo/emerald/red, Bootstrap, Material, Google blue, ChatGPT green, Claude orange, Gemini violet, 어두운 네이비 배경면)은 `COLOR_STOCK_DEFAULT`로 보고됩니다. 이 검사는 슬라이드 덱에서 항상 실행됩니다. `Canvas`에서는 기존 `Canvas` 파일의 검증 결과를 변경하지 않기 위해 모서리 둥글기, 불투명도, 그림자, 그라데이션, 패스 같은 v1.2 값을 사용할 때부터 실행됩니다. 내장 `Canvas` 템플릿은 기본 프레임워크 색상을 사용하지 않습니다.
- **다이어그램 도형**: 편집기의 도구 상자(왼쪽 패널, `Tools` 탭)에서 `Canvas`는 인코더 및 디코더 사다리꼴, 데이터(평행사변형), 저장소(실린더), 더하기(`+`가 있는 원), 곱하기(`×`가 있는 원), 분기(마름모), 고정 모듈(육각형), 텐서(직육면체)가 포함된 `다이어그램 도형` / `Diagram shapes` 그룹을 제공합니다. 에이전트는 `designllm://shapes` 또는 `design_guide` 토픽 `shapes-json`의 레시피를 사용해 동일한 도형을 `path` 노드(`changes_apply`, `add_node`)로 추가하며, `add.shape` GUI 명령은 사용자 조작이나 `ui_command_run`에서 동일한 작업을 수행합니다.

## 요구 사항

- Windows 10 또는 11, 64비트.
- 저장소 복제를 위한 Git(바로 실행 가능한 빌드가 저장소에 포함되어 있으며 용량은 약 90 MB입니다).
- 선택 사항: Microsoft Word(PDF를 편집 가능한 DOCX 사본으로 열 때만 필요).
- 선택 사항: Microsoft Edge WebView2 Runtime(Windows 11에 기본 포함, 웹 코드 미리 보기용).

## AI 클라이언트용 설치

저장소를 복제한 뒤 `install.ps1`을 실행합니다.

```powershell
git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git
cd DesignLLMTool
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

`install.ps1`은 `release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe`를 사용하며, 실행 파일이 시작되는지 확인한 뒤 `Claude Code`, `Codex CLI`, `AGY` 중 설치된 모든 클라이언트에 `designllm`이라는 이름의 MCP 서버를 등록합니다. 요청한 등록이 실패하면 종료 코드 `1`로 종료합니다.

클라이언트 등록 동작:

- **기존 등록**: 설치 스크립트는 기존 등록을 제거하거나 변경하지 않습니다. 기존 항목에는 설치 스크립트가 복원할 수 없는 설정이 들어 있을 수 있고, 사용자 설정이 아닌 프로젝트 설정에서 온 것일 수 있기 때문입니다. 이미 `designllm` 서버가 있는 클라이언트는 해당 항목을 유지합니다. 이 복사본을 등록하려면 해당 항목이 정의된 클라이언트 설정에서 그 항목을 변경하거나 제거한 뒤 `install.ps1`을 다시 실행합니다.
- **검증 및 읽을 수 없는 클라이언트**: 각 등록 후 설치 스크립트는 항목을 다시 읽어, 그 항목이 이 복사본의 `DesignLLMTool.exe`를 실행하는 경우에만 등록된 것으로 셉니다. 클라이언트가 `designllm` 서버 보유 여부를 분명하게 보고하지 않으면, 설치 스크립트는 해당 클라이언트를 변경하지 않고 미등록으로 보고합니다. 서버 목록을 읽을 수 없는 클라이언트는 변경하지 않고 그대로 둡니다.
- **`Gemini CLI`**: 설치 스크립트는 `gemini mcp add`를 실행하지 않습니다. 이 명령은 다른 서버 항목을 포함한 Gemini 설정 파일 전체를 다시 쓰며, 저장하지 않고도 성공으로 보고할 수 있기 때문입니다. 대신 `%USERPROFILE%\.gemini\settings.json`(`GEMINI_CLI_HOME` 변수가 설정된 경우 그 경로 아래의 `.gemini\settings.json`)에 추가할 `mcpServers` 항목을 출력합니다. 해당 항목을 추가하기 전에 확장 프로그램이 `designllm` 서버를 제공할 수 있으므로 `gemini mcp list`를 확인합니다.
- **`Codex CLI` 배치 심(batch shim)**: `Codex CLI`가 배치 심을 통해 실행되고 경로에 `cmd.exe`가 해석하는 문자가 포함되어 있으면, 설치 스크립트는 대신 `[mcp_servers.designllm]` TOML 항목과 클라이언트가 사용하는 Codex 설정 파일 경로(`%USERPROFILE%\.codex\config.toml`, 또는 `CODEX_HOME` 변수가 설정된 경우 그 경로 안의 `config.toml`)를 함께 출력합니다.
- **출력되는 설정**: `Gemini CLI`, `Claude Desktop`, `Cursor`, `VS Code`에 대해서는 붙여 넣을 설정을 출력합니다. 앞의 세 클라이언트에는 `mcpServers` 항목을, `VS Code`에는 `servers` 항목을 출력합니다.

설치 옵션:

- `-Project "<existing folder>"`: 해당 폴더에 작업 공간을 둡니다.
- `-Client claude|codex|gemini|agy|print`: 클라이언트 하나만 등록하거나 설정만 출력합니다.
- `-Background`: 창을 열지 않고 도구를 실행합니다.
- `-DryRun`: 아무것도 실행하거나 변경하지 않고 명령만 출력합니다.

설치 후 AI 클라이언트를 다시 시작하고 `design_guide` 도구를 호출하도록 요청합니다. 디자인 가이드 인덱스로 응답하면 연결이 정상 동작하는 것입니다.

## 수동 등록

`<repo>`를 저장소를 복제한 폴더 경로로 바꿉니다.

Claude Code:

```powershell
claude mcp add --scope user designllm -- "<repo>\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe" --mcp
```

Codex CLI(PowerShell에서는 `codex.ps1`을 실행할 때 단독 `--`가 누락되므로 `codex.cmd`를 입력합니다):

```powershell
codex.cmd mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe" --mcp
```

AGY:

```powershell
agy mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe" --mcp
```

`Gemini CLI`(`%USERPROFILE%\.gemini\settings.json`), `Claude Desktop`, `Cursor`용 `mcpServers` JSON:

```json
{
  "mcpServers": {
    "designllm": {
      "command": "<repo>\\release\\DesignLLMTool-0.1.0-mcp-win-x64\\DesignLLMTool.exe",
      "args": ["--mcp"]
    }
  }
}
```

VS Code (`.vscode/mcp.json`):

```json
{
  "servers": {
    "designllm": {
      "type": "stdio",
      "command": "<repo>\\release\\DesignLLMTool-0.1.0-mcp-win-x64\\DesignLLMTool.exe",
      "args": ["--mcp"]
    }
  }
}
```

## 서버 옵션

- `--mcp`: stdio 기반 MCP 서버로 실행합니다. 사용자가 볼 수 있도록 첫 도구 호출 시 데스크톱 창이 열립니다. 호출 시 `background: true`를 설정하거나 `--background`로 시작하면 창 없이 작업합니다.
- `--project "<existing folder>"`: 작업 공간을 `<folder>\.designllm`에 저장하고 원본 파일을 `<folder>\artifacts`에 저장합니다. 이 옵션이 없으면 작업 공간은 `%LOCALAPPDATA%\DesignLLMTool\workspaces\default`(또는 `DESIGNLLM_WORKSPACE` 환경 변수의 폴더)가 됩니다.

## 데스크톱 앱 사용

AI 없이 데스크톱 앱을 사용하려면 `DesignLLMTool.exe`를 직접 실행하거나 프로젝트 폴더를 전달합니다.

```powershell
.\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe
.\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe --project "<existing folder>"
```

- **인터페이스 언어 전환**: 앱 인터페이스는 한국어와 영어를 지원합니다. 홈 화면에서 작업 공간 메뉴(왼쪽 상단의 작업 공간 이름)를 열고 `한국어` 또는 `English`를 선택합니다. 선택 내용은 즉시 적용되며 작업 공간의 `.gui\settings.json`에 저장됩니다.
- **다이어그램 도형 추가**: `Canvas`에서 편집기의 도구 상자(왼쪽 패널, `Tools` 탭)를 열고 `다이어그램 도형` / `Diagram shapes`를 펼친 뒤 타일을 클릭하거나 페이지로 끌어 놓습니다.

## 소스에서 빌드

소스에서 직접 빌드하는 개발자용 안내입니다.

```powershell
rustup toolchain install 1.93.1
cargo +1.93.1 build --release -p designllm-application
cargo +1.93.1 test -p designllm-bridge -p designllm-contracts -p designllm-canvas -p designllm-runtimes
```

릴리스 빌드는 `target\release\DesignLLMTool.exe`를 출력합니다. 패키지 빌드를 갱신하려면 `target\release\DesignLLMTool.exe`를 `release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe` 위로 복사합니다.

## 라이선스

서드파티 라이선스 및 고지는 `release\DesignLLMTool-0.1.0-mcp-win-x64\THIRD_PARTY_LICENSES.txt`와 `release\DesignLLMTool-0.1.0-mcp-win-x64\THIRD_PARTY_NOTICES.txt`에 있습니다. Pretendard 폰트는 SIL Open Font License(`assets\fonts\OFL-Pretendard.txt`)를 따릅니다.
