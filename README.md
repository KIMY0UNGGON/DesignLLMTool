# DesignLLMTool

[한국어](README.ko.md)

DesignLLMTool is a Windows desktop design app with a built-in MCP server. AI agents (`Claude Code`, `Codex CLI`, `Gemini CLI`, `AGY`, `Claude Desktop`, `Cursor`, `VS Code`, and other MCP clients) use it to create and edit editable design artifacts while you watch in the app window.

## Contents

- [What it does](#what-it-does)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Installer and client setup](#installer-and-client-setup)
- [Server options](#server-options)
- [Desktop app](#desktop-app)
- [Build from source](#build-from-source)
- [Licences](#licences)

## What it does

| Artifact kind | Used for | Saved as |
| --- | --- | --- |
| `Canvas` | Screens, landing pages, posters, social posts, paper architecture diagrams | Native file |
| `Slides` | Decks | `PPTX` |
| `Figure` | Data plots from CSV with evidence from PDF | Native file |
| `DOCX`, `HWPX`, `HWP` | Documents | Their own format |
| `Web code` | Web projects | Native file |

Exports: `PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, and `ZIP` (web code). An engine refuses a format it does not write.

### Design guides

- `design_guide` returns built-in guides, and topic `glossary` defines every design term they use.
- A change (including `changes_validate`) sent before the connection receives an artifact's guide is refused with `GUIDE_REQUIRED`, carrying that guide.
- Results from `artifact_create`, `artifact_open`, `artifact_inspect`, `context_read`, and `artifact_validate` also carry the guide.
- Once delivered, a guide covers every artifact on that connection using the same guide (for example, every deck). Calling `design_guide` with only a topic is reference reading and unlocks nothing.
- Applied changes to a deck, or to a `Canvas` using a v1.2 value, return design-quality warnings; warnings triggered in another artifact after a shared token change name that artifact.

### Paper diagrams

- Follow the `paper-diagram` guide: use a `Canvas` at print size with no title inside the figure, typed shapes (trapezoid encoders, cylinders, `circle_plus` residual joins), orthogonal arrows, and `N×` repeat frames.
- `design_guide` topic `paper-diagram-example` returns a complete Transformer encoder figure as operations for one `changes_apply`.

### Paper figures

- Follow the `paper-figure` guide: use a `Figure` artifact, Okabe-Ito colours, and axis labels with units.
- Diagrams inside a `Figure` support only rectangles, ellipses, math formulas, and straight connectors; draw elbows and typed shapes on a `Canvas`.
- The `figure.output_profile` command sets a profile (single column, 1.5 column, double column, high-resolution print, or presentation 16:9).

<details>
<summary>Figure output profile colour rules</summary>

- `figure.output_profile` assigns its first palette colour to every plot panel so the same series matches across panels.
- Later series follow palette order (Okabe-Ito, starting with near-black for high-resolution print and presentation), and unambiguous common-legend swatches are recoloured to match their series.
- Without a chosen profile, a `Figure` keeps earlier colours unless a plot has two or more series and either its panel colour is in an output profile palette or a later series repeated the panel colour; that plot receives the current rule's series colours and requires visual approval again.

</details>

### Design-quality rules

- Layout explains structure and auxiliary labels are omitted. Top or left card/box accent bars (the blog-callout look) are reported as `ACCENT_BAR` unless requested.
- Stock framework and assistant colours (Tailwind blue/indigo/emerald/red, Bootstrap, Material, Google blue, ChatGPT green, Claude orange, Gemini violet, dark navy surfaces) are reported by `COLOR_STOCK_DEFAULT`. Built-in `Canvas` templates use no stock framework colours.
- Decks are always checked. A `Canvas` receives all design-quality warnings (including `ACCENT_BAR` and `COLOR_STOCK_DEFAULT`) only once it uses a v1.2 value (corner radius, opacity, shadow, gradient, or path), keeping older `Canvas` validation output unchanged.

### Diagram shapes

- In the editor toolbox (left panel, `도구` / `Tools` tab), `다이어그램 도형` / `Diagram shapes` provides encoder and decoder trapezoids, data (parallelogram), store (cylinder), add (circle with `+`), multiply (circle with `×`), branch (diamond), frozen module (hexagon), and tensor (cuboid).
- Agents add these shapes as `path` nodes (`changes_apply`, `add_node`) from `designllm://shapes` or `design_guide` topic `shapes-json`; the `add.shape` GUI command does the same for a person or `ui_command_run`.

## Requirements

- Windows 10 or 11, 64-bit.
- Git, to clone the repository (the ready-to-run build is in the repository; about 115 MB checked out).
- Optional: Microsoft Word, only for opening a PDF as an editable DOCX copy.
- Optional: Microsoft Edge WebView2 Runtime (included in Windows 11), for the web-code preview.

## Quick start

1. Clone the repository:

   ```powershell
   git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git
   cd DesignLLMTool
   ```

2. Run `install.ps1`:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```

   `install.ps1` uses `release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe`, verifies that it starts, and registers the MCP server `designllm` with every installed client among `Claude Code`, `Codex CLI`, and `AGY`. It exits with code `1` when a requested registration fails.

3. Restart your AI client and ask it to call the `design_guide` tool.

> [!TIP]
> If `design_guide` returns the design guide index, the connection works.

## Installer and client setup

> [!IMPORTANT]
> The installer never removes or changes an existing `designllm` entry. Change or remove that entry in the client settings where it is defined, then run `install.ps1` again.

> [!NOTE]
> For `Gemini CLI`, `install.ps1` prints the `mcpServers` entry instead of registering it. Check `gemini mcp list` first because an extension can provide `designllm`.

### Installer options

| Flag | Effect |
| --- | --- |
| `-Project "<existing folder>"` | Keep the workspace in that folder. |
| `-Client claude\|codex\|gemini\|agy\|print` | Register one client, or only print the configuration. |
| `-Background` | Tools run without opening the window. |
| `-DryRun` | Print the commands without running or changing anything. |

### Client configuration overview

| Client | Method | Config format / target |
| --- | --- | --- |
| `Claude Code`, `Codex CLI`, `AGY` | Registered by `install.ps1` | Client CLI MCP registry |
| `Gemini CLI` | Printed by `install.ps1` | `mcpServers` JSON in `%USERPROFILE%\.gemini\settings.json` (or `.gemini\settings.json` under `GEMINI_CLI_HOME`) |
| `Claude Desktop`, `Cursor` | Printed by `install.ps1` | `mcpServers` JSON |
| `VS Code` | Printed by `install.ps1` | `servers` JSON in `.vscode/mcp.json` |

<details>
<summary>Installer behaviour details</summary>

- **Existing registrations**: The installer never removes or changes an existing `designllm` entry, because it can hold settings the installer cannot restore or come from project settings. Change or remove that entry in the client settings where it is defined, then run `install.ps1` again.
- **Verification and unreadable clients**: After each registration, the installer reads the entry back and counts it as registered only when it runs this copy's `DesignLLMTool.exe`. If a client does not report clearly or its server list cannot be read, the installer leaves it unchanged and reports it as not registered.
- **`Gemini CLI`**: The installer skips `gemini mcp add` because it rewrites the entire Gemini settings file (including other servers) and can report success without saving. It prints the `mcpServers` entry instead; check `gemini mcp list` first because an extension can provide `designllm`.
- **Batch shims**: When a client runs through a `.cmd` or `.bat` shim and a path contains a character `cmd.exe` would interpret, the installer leaves that client unregistered, reports it as not registered, and prints the configuration. For `Codex CLI`, it also prints a `[mcp_servers.designllm]` TOML entry and the Codex settings path (`%USERPROFILE%\.codex\config.toml`, or `config.toml` in `CODEX_HOME`).
- **Printed configurations**: For `Gemini CLI`, `Claude Desktop`, `Cursor`, and `VS Code`, the installer prints the JSON entry to paste (`mcpServers` for the first three; `servers` for `VS Code`).

</details>

<details>
<summary>Manual registration commands per client</summary>

Replace `<repo>` with the clone folder path.

Claude Code:

```powershell
claude mcp add --scope user designllm -- "<repo>\release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe" --mcp
```

Codex CLI (type `codex.cmd` in PowerShell, because PowerShell drops the bare `--` when it runs `codex.ps1`):

```powershell
codex.cmd mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe" --mcp
```

AGY:

```powershell
agy mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe" --mcp
```

`Gemini CLI` (`%USERPROFILE%\.gemini\settings.json`), `Claude Desktop`, and `Cursor` (`mcpServers` JSON):

```json
{
  "mcpServers": {
    "designllm": {
      "command": "<repo>\\release\\DesignLLMTool-0.1.1-mcp-win-x64\\DesignLLMTool.exe",
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
      "command": "<repo>\\release\\DesignLLMTool-0.1.1-mcp-win-x64\\DesignLLMTool.exe",
      "args": ["--mcp"]
    }
  }
}
```

</details>

## Server options

| Flag | Effect |
| --- | --- |
| `--mcp` | Run as an MCP server over stdio. The desktop window opens on the first tool call; pass `background: true` on a call or start with `--background` to run without the window. |
| `--project "<existing folder>"` | Store the workspace in `<folder>\.designllm` and save native files to `<folder>\artifacts`. Without it, the workspace is `%LOCALAPPDATA%\DesignLLMTool\workspaces\default` (or the folder in `DESIGNLLM_WORKSPACE`). |

## Desktop app

Run `DesignLLMTool.exe` directly or pass a project folder to use the app without AI:

```powershell
.\release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe
.\release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe --project "<existing folder>"
```

- **Switch interface language**: On the home screen, open the workspace menu (workspace name at the top left) and choose `한국어` or `English`. The setting applies immediately and is saved in `.gui\settings.json` in the workspace.
- **Add diagram shapes**: On a `Canvas`, open the editor toolbox (left panel, `도구` / `Tools` tab), find `다이어그램 도형` / `Diagram shapes`, and click or drag a tile onto the page.

## Build from source

```powershell
rustup toolchain install 1.93.1
cargo +1.93.1 build --release -p designllm-application
cargo +1.93.1 test -p designllm-bridge -p designllm-contracts -p designllm-canvas -p designllm-runtimes
```

The release build outputs `target\release\DesignLLMTool.exe`. Copy it over `release\DesignLLMTool-0.1.1-mcp-win-x64\DesignLLMTool.exe` to update the packaged build.

## Licences

Third-party licences and notices are in `release\DesignLLMTool-0.1.1-mcp-win-x64\THIRD_PARTY_LICENSES.txt` and `release\DesignLLMTool-0.1.1-mcp-win-x64\THIRD_PARTY_NOTICES.txt`. The Pretendard font is under the SIL Open Font License (`assets\fonts\OFL-Pretendard.txt`).
