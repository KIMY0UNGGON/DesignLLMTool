# DesignLLMTool

[한국어](README.ko.md)

DesignLLMTool is a desktop design app for Windows, macOS, and Linux with a built-in MCP server. AI agents (`Claude Code`, `Codex CLI`, `Gemini CLI`, `AGY`, `Claude Desktop`, `Cursor`, `VS Code`, and other MCP clients) use it to create and edit editable design artifacts while you watch in the app window.

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

- Layout explains structure and auxiliary labels are omitted. Any coloured bar along one edge of a card or box (top, bottom, left, or right) is reported as `ACCENT_BAR` unless requested.
- Stock framework and assistant colours (Tailwind blue/indigo/emerald/red, Bootstrap, Material, Google blue, ChatGPT green, Claude orange, Gemini violet, dark navy surfaces) are reported by `COLOR_STOCK_DEFAULT`. Built-in `Canvas` templates use no stock framework colours, and the built-in landing page template's feature cards have no shadow under their outline.
- `ICON_EMOJI` now also reports a check, cross, star, sparkle, or arrow glyph (`✓ ✗ ★ ✦ →`) followed by a space and text, used in place of an icon.
- Additional non-blocking warnings: `SHADOW_HARD_OFFSET` (a shadow with no blur cast 2 px or more away, at least 0.25 opaque: the neobrutalist block shadow), `SHADOW_GLOW` (a coloured shadow with no offset, a coloured halo), `CARD_GHOST` (an outline of 2 px or less under a shadow blurred 16 px or more; show elevation once—new shapes carry a 1 pt grey outline by default, so set `stroke_width` to 0 on a card that keeps its shadow), `CARD_NESTED` (a shadowed card inside a larger shadowed card), `TEXT_EYEBROW` (a short capitals or letter-spaced label right above a heading), and `TEXT_SECTION_NUMBER` (decorative `01`, `02`, `03` on one page).
- Built-in design guidance tells agents to avoid, where the request allows: a big-number hero metric, monospace type as a costume for text that is not code or data, Impact or Arial Black as the display face, stripe or grid textures with no subject behind them, and invented numbers; to show elevation once; and to choose a light or dark page from where the work is used. The user's request wins over these defaults.
- These rules paraphrase the anti-patterns of the open-source design skill [impeccable](https://github.com/pbakaus/impeccable) (Apache-2.0).
- Decks are always checked. A `Canvas` receives all non-blocking design-quality warnings (including `ACCENT_BAR` and `COLOR_STOCK_DEFAULT`) only once it uses a v1.2 value (corner radius, opacity, shadow, gradient, or path), keeping older `Canvas` validation output unchanged.

### Diagram shapes

- In the editor toolbox (left panel, `도구` / `Tools` tab), `다이어그램 도형` / `Diagram shapes` provides encoder and decoder trapezoids, data (parallelogram), store (cylinder), add (circle with `+`), multiply (circle with `×`), branch (diamond), frozen module (hexagon), and tensor (cuboid).
- Agents add these shapes as `path` nodes (`changes_apply`, `add_node`) from `designllm://shapes` or `design_guide` topic `shapes-json`; the `add.shape` GUI command does the same for a person or `ui_command_run`.

## Requirements

- **Windows**:
  - Windows 10 or 11, 64-bit.
  - Git, to clone the repository (the ready-to-run build is in the repository; about 115 MB checked out).
  - Optional (Windows-only): Microsoft Word, only for opening a PDF as an editable DOCX copy.
  - Optional (Windows-only): Microsoft Edge WebView2 Runtime (included in Windows 11), for the web-code preview and automatic video poster extraction.
- **macOS and Linux**:
  - macOS 11 or later (Apple silicon and Intel), or Linux (`x64` and `arm64`) with an X11 or Wayland desktop.
  - Git, to clone the repository (there is no packaged binary in the repository for macOS or Linux; `install.sh` downloads the release package), plus `curl` or `wget`, `tar`, and `sha256sum` or `shasum` (all present on macOS and on desktop Linux distributions).
  - Needed only for a source build (`--build`, or when the download fails): Rust toolchain (`1.93.1`, from [rustup.rs](https://rustup.rs)) and a C compiler because SQLite is compiled from source (Xcode Command Line Tools on macOS via `xcode-select --install`, or `build-essential` or equivalent on Linux).
  - Linux at run time: glibc 2.35 or later (Ubuntu 22.04 or later, Debian 12 or later, Fedora 36 or later) for the downloaded executable (on an older system the downloaded executable does not start; use `./install.sh --build`), an X11 or Wayland session with OpenGL or Vulkan drivers, `zenity` or `kdialog` for file open/save dialogs, and the keyboard library `libxkbcommon-x11` (and `libxkbcommon` on Wayland), which the desktop window loads at run time and without which the window cannot open; desktop installs of Ubuntu, Fedora, and similar include it, a minimal install can add it with the distribution package (`libxkbcommon-x11-0` on Debian and Ubuntu), and it is needed only for the window (`--mcp --background` runs without it).

## Quick start

1. Clone the repository:

   ```powershell
   git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git
   cd DesignLLMTool
   ```

2. Run the installer:

   Windows (`install.ps1`):

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install.ps1
   ```

   `install.ps1` uses `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe`, verifies that it starts, and registers the MCP server `designllm` with every installed client among `Claude Code`, `Codex CLI`, and `AGY`. It exits with code `1` when a requested registration fails.

   macOS and Linux (`install.sh`):

   ```bash
   ./install.sh
   ```

   `install.sh` works without Rust: when `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool` (`<os>` is `macos` or `linux`; `<arch>` is `x64` or `arm64`) is not in the clone, it downloads `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz` from the GitHub release [`v0.1.2`](https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2) (with `curl`, or `wget`), checks it against `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256`, refuses an archive with files outside the package folder, and unpacks it into `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/` (ignored by git; later runs reuse it without downloading; `DESIGNLLM_RELEASE_URL` replaces the download folder, defaulting to `https://github.com/KIMY0UNGGON/DesignLLMTool/releases/download/v<version>` where `<version>` is the version in `Cargo.toml`). Only when the download fails (no network, no release for that version, checksum mismatch), or with `--build`, `install.sh` builds from source with Rust `1.93.1` (through `rustup` when present, otherwise `cargo` on `PATH`) into `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`; with `--client print`, a failed download prints the build hint instead of building. It verifies that it starts and registers the MCP server `designllm` with every installed client among `Claude Code`, `Codex CLI`, and `AGY`, exiting with code `1` when a requested registration fails. On Linux without `DISPLAY` or `WAYLAND_DISPLAY`, it warns to use `--background`.

3. Restart your AI client and ask it to call the `design_guide` tool.

> [!TIP]
> If `design_guide` returns the design guide index, the connection works.

## Installer and client setup

> [!IMPORTANT]
> When an existing `designllm` entry runs an executable from this same clone (`release/DesignLLMTool-*-mcp-*/` or `target/package/DesignLLMTool-*-mcp-*/`, for example an older version), the installer replaces it with the current executable and reports it as updated (unless its arguments differ from this run's options, or for `Claude Code` it is not in user scope). Entries pointing anywhere else (another clone, another program) are never changed; for those, change or remove that entry in the client settings where it is defined, then run `install.ps1` (or `install.sh`) again.

> [!NOTE]
> For `Gemini CLI`, `install.ps1` and `install.sh` print the `mcpServers` entry instead of registering it. Check `gemini mcp list` first because an extension can provide `designllm`.

### Installer options

| `install.ps1` | `install.sh` | Effect |
| --- | --- | --- |
| `-Project "<existing folder>"` | `--project DIR` | Keep the workspace in that folder. |
| `-Client claude\|codex\|gemini\|agy\|print` | `--client auto\|claude\|codex\|gemini\|agy\|print` | Register one client, or only print the configuration. |
| `-Background` | `--background` | Tools run without opening the window. |
| `-DryRun` | `--dry-run` | Print the commands without running or changing anything (`--dry-run` downloads nothing). |
| — | `--build` | Build from source into `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/` instead of downloading or reusing a packaged binary under `release/`. |

### Client configuration overview

| Client | Method | Config format / target |
| --- | --- | --- |
| `Claude Code`, `Codex CLI`, `AGY` | Registered by `install.ps1` / `install.sh` | Client CLI MCP registry |
| `Gemini CLI` | Printed by `install.ps1` / `install.sh` | `mcpServers` JSON in `%USERPROFILE%\.gemini\settings.json` (or `.gemini\settings.json` under `GEMINI_CLI_HOME`) on Windows; `~/.gemini/settings.json` on macOS and Linux |
| `Claude Desktop`, `Cursor` | Printed by `install.ps1` / `install.sh` | `mcpServers` JSON |
| `VS Code` | Printed by `install.ps1` / `install.sh` | `servers` JSON in `.vscode/mcp.json` |

<details>
<summary>Installer behaviour details</summary>

- **Existing registrations**: An existing `designllm` entry whose command is an executable of this same clone (`release/DesignLLMTool-*-mcp-*/` or `target/package/DesignLLMTool-*-mcp-*/`, for example an older version) is replaced with the current executable and reported as updated; environment variables set by hand on a replaced entry are not carried over. It is kept, with a message, when its arguments differ from this run's options, or for `Claude Code` when the entry is not in user scope. Entries pointing anywhere else (another clone, another program) are never changed, because they can hold settings the installer cannot restore or come from project settings; for those, change or remove that entry in the client settings where it is defined, then run `install.ps1` (or `install.sh`) again.
- **Verification and unreadable clients**: After each registration, the installer reads the entry back and counts it as registered only when it runs this copy's `DesignLLMTool.exe` (or `DesignLLMTool` on macOS and Linux). If a client does not report clearly or its server list cannot be read, the installer leaves it unchanged and reports it as not registered.
- **`Gemini CLI`**: The installer skips `gemini mcp add` because it rewrites the entire Gemini settings file (including other servers) and can report success without saving. It prints the `mcpServers` entry instead; check `gemini mcp list` first because an extension can provide `designllm`.
- **Batch shims**: On Windows, when a client runs through a `.cmd` or `.bat` shim and a path contains a character `cmd.exe` would interpret, the installer leaves that client unregistered, reports it as not registered, and prints the configuration. For `Codex CLI`, it also prints a `[mcp_servers.designllm]` TOML entry and the Codex settings path (`%USERPROFILE%\.codex\config.toml`, or `config.toml` in `CODEX_HOME`).
- **Printed configurations**: For `Gemini CLI`, `Claude Desktop`, `Cursor`, and `VS Code`, the installer prints the JSON entry to paste (`mcpServers` for the first three; `servers` for `VS Code`).
- **macOS notarization**: The macOS executables are not notarized. `install.sh` downloads with `curl` or `wget`, which sets no quarantine flag, so they start. If you download the archive in a web browser instead, macOS blocks the executable until you remove the flag with `xattr -d com.apple.quarantine DesignLLMTool`.

</details>

<details>
<summary>Manual registration commands per client</summary>

Replace `<repo>` with the clone folder path.

**Windows**:

Claude Code:

```powershell
claude mcp add --scope user designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

Codex CLI (type `codex.cmd` in PowerShell, because PowerShell drops the bare `--` when it runs `codex.ps1`):

```powershell
codex.cmd mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

AGY:

```powershell
agy mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

`Gemini CLI` (`%USERPROFILE%\.gemini\settings.json`), `Claude Desktop`, and `Cursor` (`mcpServers` JSON):

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

VS Code (`.vscode/mcp.json`):

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

**macOS and Linux** (`<os>` is `macos` or `linux`; `<arch>` is `x64` or `arm64`):

The paths below use `<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool` (the folder `install.sh` downloads into); a source build is at `<repo>/target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool`.

Manual install without `install.sh`: download `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz` and `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256` from the GitHub release [`v0.1.2`](https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2), verify the checksum (`shasum -a 256 -c DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256`), unpack it (`tar -xzf DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz`), and register `<folder>/DesignLLMTool --mcp` with the client. The archive holds `DesignLLMTool-0.1.2-mcp-<os>-<arch>/` with `DesignLLMTool`, `assets/fonts`, `THIRD_PARTY_LICENSES.txt`, and `THIRD_PARTY_NOTICES.txt`; keep the unpacked folder together because the executable needs `assets/fonts` next to it. On macOS, if downloaded in a web browser, run `xattr -d com.apple.quarantine DesignLLMTool` before starting it.

Claude Code, Codex CLI (no `codex.cmd` needed), and AGY:

```bash
claude mcp add --scope user designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
codex mcp add designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
agy mcp add designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
```

`Gemini CLI` (`~/.gemini/settings.json`), `Claude Desktop`, and `Cursor` (`mcpServers` JSON):

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

VS Code (`.vscode/mcp.json`):

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

### Updating

In the clone, run `git pull`, then run the installer again (`install.ps1` on Windows, `./install.sh` on macOS and Linux) with the same options used for the first install (for example the same `-Project` / `--project` and `-Background` / `--background`), then restart the AI client.

## Server options

| Flag | Effect |
| --- | --- |
| `--mcp` | Run as an MCP server over stdio. The desktop window opens on the first tool call; pass `background: true` on a call or start with `--background` to run without the window. |
| `--project "<existing folder>"` | Store the workspace in `<folder>\.designllm` and save native files to `<folder>\artifacts`. Without it, the default workspace is `%LOCALAPPDATA%\DesignLLMTool\workspaces\default` on Windows, `~/Library/Application Support/DesignLLMTool/workspaces/default` on macOS, and `$XDG_DATA_HOME/DesignLLMTool/workspaces/default` (defaulting to `~/.local/share/DesignLLMTool/workspaces/default`) on Linux; `DESIGNLLM_WORKSPACE` overrides this on every platform. |

- **Update check (`--mcp`)**: On `--mcp`, a background check asks the GitHub releases API for the latest release at most once every 24 hours and caches the answer in `DesignLLMTool/update-check.json` inside the per-user app data folder (`%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS, `$XDG_DATA_HOME` or `~/.local/share` on Linux). It never delays the MCP handshake or tool calls, and a failed request is silent. When a newer release exists, the first successful tool result on a connection carries, once per connection, a notice naming both versions and how to update, plus `structuredContent.update_available` (`latest_version`, `current_version`, `url`, `how_to_update`). Set `DESIGNLLM_NO_UPDATE_CHECK=1` to turn it off.

### Platform differences

- **Windows-only features** (not available on macOS or Linux):
  - Web-code preview window (requires WebView2); export the web code as `HTML` or `ZIP` and open it in a browser instead.
  - Automatic video poster extraction (requires WebView2).
  - Opening a PDF as an editable `DOCX` copy (requires Microsoft Word COM automation).
- **Everything else**: artifact kinds, editing, rendering, and exports (`PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, `ZIP`, `DOCX`, `HWPX`, `HWP`) work the same on every platform.
- **Host connection**: Named pipe on Windows; on macOS and Linux, a Unix domain socket in a directory only the user can open (`$XDG_RUNTIME_DIR`, or `<temp>/dllm-<uid>` with mode `0700`), so every client must run as the same user.
- **AI job processes**: Windows job object on Windows; on macOS and Linux, a process group per job, ended together with `SIGKILL`.
- **Graphics**: OpenGL first, with `wgpu` fallback on the native backend (DirectX 12 on Windows, Metal on macOS, Vulkan on Linux); `DESIGNLLM_RENDERER=glow|wgpu|wgpu-warp` forces one.

## Desktop app

Run the app directly or pass a project folder to use the app without AI:

Windows:

```powershell
.\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe
.\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe --project "<existing folder>"
```

macOS and Linux (for a source build, replace `release` with `target/package`):

```bash
./release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool
./release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool --project "<existing folder>"
```

- **Switch interface language**: On the home screen, open the workspace menu (workspace name at the top left) and choose `한국어` or `English`. The setting applies immediately and is saved in `.gui\settings.json` in the workspace.
- **Add diagram shapes**: On a `Canvas`, open the editor toolbox (left panel, `도구` / `Tools` tab), find `다이어그램 도형` / `Diagram shapes`, and click or drag a tile onto the page.

## Build from source

```powershell
rustup toolchain install 1.93.1
cargo +1.93.1 build --release -p designllm-application
cargo +1.93.1 test -p designllm-bridge -p designllm-contracts -p designllm-canvas -p designllm-runtimes
```

The release build outputs `target\release\DesignLLMTool.exe` on Windows (copy it over `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe` to update the packaged build) and `target/release/DesignLLMTool` on macOS and Linux, which needs `assets/fonts` next to the executable (`install.sh --build`, or `install.sh` when the download fails, lays out `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`).

The GitHub Actions workflow (`.github/workflows/build.yml`) builds and tests on Windows, macOS (`arm64` and `x64`), and Linux (`x64` and `arm64`), uploads `DesignLLMTool-<version>-mcp-<platform>.tar.gz` packages as workflow artifacts, writes the `.sha256` file, and attaches both files to the GitHub release when a `v*` tag is pushed. Workflow `.github/workflows/install-check.yml` (run by hand, or when a release is published) installs from a fresh clone on macOS (`arm64`, `x64`) and Linux (`x64`, `arm64`) with `cargo` and `rustup` removed from `PATH`, checks that `install.sh` unpacked the release package and built nothing, and runs an MCP session against it with `DESIGNLLM_NO_UPDATE_CHECK=1`.

## Licences

Third-party licences and notices are in `release\DesignLLMTool-0.1.2-mcp-win-x64\THIRD_PARTY_LICENSES.txt` and `release\DesignLLMTool-0.1.2-mcp-win-x64\THIRD_PARTY_NOTICES.txt`. The Pretendard font is under the SIL Open Font License (`assets\fonts\OFL-Pretendard.txt`).
