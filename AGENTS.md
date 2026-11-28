# AGENTS.md

Instructions for AI agents installing, connecting to, and working with DesignLLMTool.

## Install and connect

1. Verify the host meets the requirements:
   - **Windows**: Windows 10 or 11 (64-bit) and Git. Optional (Windows-only): Microsoft Word (only for opening a PDF as an editable DOCX copy) and Microsoft Edge WebView2 Runtime (included in Windows 11, for web-code preview and automatic video poster extraction).
   - **macOS and Linux**: macOS 11 or later (Apple silicon and Intel) or Linux (`x64` and `arm64`) with an X11 or Wayland desktop, Git, plus `curl` or `wget`, `tar`, and `sha256sum` or `shasum` (all present on macOS and on desktop Linux distributions). Rust toolchain (`1.93.1`, from `https://rustup.rs`) and a C compiler because SQLite is compiled from source (Xcode Command Line Tools on macOS via `xcode-select --install`, or `build-essential` or equivalent on Linux) are needed on macOS and Linux only for a source build (`--build`, or when the download fails). Linux downloaded executables need glibc 2.35 or later (Ubuntu 22.04 or later, Debian 12 or later, Fedora 36 or later); on an older system the downloaded executable does not start, so use `./install.sh --build`. Linux at run time also requires an X11 or Wayland session with OpenGL or Vulkan drivers, `zenity` or `kdialog` for file open/save dialogs, and the keyboard library `libxkbcommon-x11` (and `libxkbcommon` on Wayland), which the desktop window loads at run time and without which the window cannot open; desktop installs of Ubuntu, Fedora, and similar include it, a minimal install can add it with the distribution package (`libxkbcommon-x11-0` on Debian and Ubuntu), and it is needed only for the window (`--mcp --background` runs without it).
2. Clone the repository and enter the clone directory (about 115 MB checked out). On Windows, it contains the ready-to-run binary `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe`. On macOS and Linux, there is no packaged binary in the repository and `install.sh` downloads the release package (falling back to a source build only when the download fails or `--build` is passed):

```powershell
git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git
cd DesignLLMTool
```

3. Optionally ask the user which existing project folder to use for the workspace. With a project folder, the workspace lives in `<folder>\.designllm` and native files are saved to `<folder>\artifacts`. Without it, the default workspace is `%LOCALAPPDATA%\DesignLLMTool\workspaces\default` on Windows, `~/Library/Application Support/DesignLLMTool/workspaces/default` on macOS, and `$XDG_DATA_HOME/DesignLLMTool/workspaces/default` (defaulting to `~/.local/share/DesignLLMTool/workspaces/default`) on Linux; `DESIGNLLM_WORKSPACE` overrides this on every platform.
4. Run the installer (`install.ps1` on Windows; `install.sh` on macOS and Linux). On Windows, `install.ps1` verifies that `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe` starts. On macOS and Linux, `install.sh` works without Rust: when `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool` (`<os>` is `macos` or `linux`; `<arch>` is `x64` or `arm64`) is not in the clone, it downloads `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz` from the GitHub release `v0.1.2` (`https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2`) with `curl` or `wget` (`DESIGNLLM_RELEASE_URL` replaces the download folder, defaulting to `https://github.com/KIMY0UNGGON/DesignLLMTool/releases/download/v<version>` where `<version>` is the version in `Cargo.toml`), checks it against `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256`, refuses an archive with files outside the package folder, and unpacks it into `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/` (containing `DesignLLMTool`, `assets/fonts`, `THIRD_PARTY_LICENSES.txt`, and `THIRD_PARTY_NOTICES.txt`; ignored by git; later runs reuse it without downloading). Only when the download fails (no network, no release for that version, checksum mismatch), or with `--build`, `install.sh` builds from source with Rust `1.93.1` (through `rustup` when present, otherwise `cargo` on `PATH`) into `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`; with `--client print`, a failed download prints the build hint instead of building. On macOS, the executables are not notarized: `install.sh` uses `curl`/`wget`, which sets no quarantine flag so they start, whereas a browser download requires `xattr -d com.apple.quarantine DesignLLMTool`. Both installers verify that the executable starts and register the MCP server `designllm` with every installed client among `Claude Code`, `Codex CLI`, and `AGY`. For `Gemini CLI`, they only print the `mcpServers` entry because `gemini mcp add` rewrites the entire settings file and can report success without saving. On Linux without `DISPLAY` or `WAYLAND_DISPLAY`, `install.sh` warns to use `--background`:

   Windows (`install.ps1`):

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

   macOS and Linux (`install.sh`):

```bash
./install.sh
```

   Available installer flags:
   - `-Project "<existing folder>"` / `--project DIR`: keep the workspace in that folder.
   - `-Client claude|codex|gemini|agy|print` / `--client auto|claude|codex|gemini|agy|print`: target one client, or print configuration only.
   - `-Background` / `--background`: tools run without opening the app window.
   - `-DryRun` / `--dry-run`: print commands without running or changing anything (`--dry-run` downloads nothing).
   - `--build` (`install.sh` only): build from source into `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/` instead of downloading or reusing a packaged binary under `release/`.

   Note: if an existing `designllm` registration runs an executable of this same clone (`release/DesignLLMTool-*-mcp-*/` or `target/package/DesignLLMTool-*-mcp-*/`, for example an older version), the installer replaces it with the current executable and reports it as updated; environment variables set by hand on a replaced entry are not carried over. It keeps the entry, with a message, when its arguments differ from this run's options, or for `Claude Code` when the entry is not in user scope. Entries pointing anywhere else (another clone, another program) are never changed; to register this copy instead, change or remove that existing entry in the client settings where it is defined (user or project), then run `install.ps1` (or `install.sh`) again. The installer counts a registration only when reading the entry back shows this copy's `DesignLLMTool.exe` (or `DesignLLMTool` on macOS and Linux). If a client does not report its server list clearly, the installer leaves that client unchanged. When a requested registration fails, the installer exits with code `1`. On Windows, when a client runs through a batch shim (`.cmd` or `.bat`) and a path contains a character that `cmd.exe` would read, the installer does not register that client, reports it as not registered, and leaves it to the printed configuration; for `Codex CLI` it also prints a `[mcp_servers.designllm]` TOML entry together with the path to the Codex settings file that the client uses (`%USERPROFILE%\.codex\config.toml`, or `config.toml` in `CODEX_HOME` when that variable is set).

5. If a CLI is missing, or when setting up `Claude Desktop`, `Cursor`, or `VS Code`, print the JSON configuration and tell the user where to paste it:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Client print
```

```bash
./install.sh --client print
```

   - For `Gemini CLI`, `Claude Desktop`, and `Cursor`, paste the printed `mcpServers` JSON entry into the client's MCP configuration unless that client already has a `designllm` server (for `Gemini CLI`, check `gemini mcp list` because an extension can provide one). For `Gemini CLI`, use `%USERPROFILE%\.gemini\settings.json` or the settings file under `GEMINI_CLI_HOME` on Windows, and `~/.gemini/settings.json` on macOS and Linux.
   - For `VS Code`, paste the printed `servers` JSON entry into `.vscode/mcp.json`.
   - For manual CLI registration on Windows (replacing `<repo>` with the clone folder), use `codex.cmd` in PowerShell for Codex CLI so PowerShell does not drop the bare `--`:

```powershell
claude mcp add --scope user designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
codex.cmd mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
agy mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe" --mcp
```

   - For manual CLI registration on macOS and Linux (`<os>` is `macos` or `linux`; `<arch>` is `x64` or `arm64`, no `codex.cmd` needed), point to `<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool` (the folder `install.sh` downloads into; a source build is at `<repo>/target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool`):

```bash
claude mcp add --scope user designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
codex mcp add designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
agy mcp add designllm -- "<repo>/release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/DesignLLMTool" --mcp
```

   - For manual install without `install.sh` on macOS and Linux: download `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz` and `DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256` from the release (`https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.1.2`), check it (`shasum -a 256 -c DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz.sha256`), unpack it (`tar -xzf DesignLLMTool-0.1.2-mcp-<os>-<arch>.tar.gz`), and register `<folder>/DesignLLMTool --mcp` with the client. The executable needs `assets/fonts` next to it, so keep the unpacked folder together (and on macOS, if downloaded in a web browser, run `xattr -d com.apple.quarantine DesignLLMTool`).

6. Verify the connection: have the user restart the AI client, then call `design_guide`. If it returns the design guide index, the connection works.

### Updating

In the clone, run `git pull`, then run the installer again (`install.ps1` on Windows; `./install.sh` on macOS and Linux) with the same options used for the first install (for example the same `-Project` / `--project` and `-Background` / `--background`), then restart the AI client.

## How to work with DesignLLMTool

### Standard workflow

1. Call `workspace_list`.
2. Before making the first change to an artifact, call `design_guide` with its `artifact_id`. That artifact-bound guide—or the guide attached to an `artifact_create`, `artifact_open`, `artifact_inspect`, `context_read`, or `artifact_validate` result, or carried by a `GUIDE_REQUIRED` refusal—unlocks changes on the connection for that artifact and for every other artifact that uses the same guide (for example, every deck). Calling `design_guide` with only a topic (`paper-diagram`, `paper-diagram-example`, `paper-figure`, `slides`, `glossary`, `shapes`, `shapes-json`) is reference reading and unlocks nothing. Follow the guide; if the user's explicit instructions conflict with it, the user's instructions win.
3. Create or open the artifact (`artifact_create`, `artifact_open`), read it with `artifact_inspect`, and apply edits with `changes_validate` then `changes_apply`.
4. Read the design-quality warnings in each `changes_apply` result and fix them; inspect the rendered output with `render_get`.
5. Deliver with `artifact_export` (`PDF`, `SVG`, `PNG`, `PPTX`, `HTML`) or `artifact_save` (native file).

### MCP resources and prompts

- `designllm://manual/index`: the guide index; each guide is its own `designllm://manual/<slug>` resource.
- `designllm://shapes`: shape recipes.
- `designllm://presets`: frame sizes.
- MCP prompts: one per task guide.

### Artifacts, guides, and quality rules

- **Artifact kinds and exports**: `Canvas` (screens, landing pages, posters, social posts, paper architecture diagrams), `Slides` (saved as PPTX), `Figure` (data plots from CSV with evidence from PDF), `DOCX`, `HWPX`, and `HWP` documents, and `Web code`. Export via `PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, and `ZIP` (web code); documents save in their native format.
- **Guide enforcement**: if a change (`changes_validate`, `changes_apply`, `proposal_create`, `proposal_apply`, `design_themes_apply`, `figure_dataset_edit`, `design_system_set`, or `design_system_publish`) is sent to an artifact before the connection has received its guide, that first call is refused with `GUIDE_REQUIRED`, carrying the guide. After each applied change to a deck, or to a `Canvas` that uses a v1.2 value (see the anti-generic rules below), the response lists design-quality warnings; if a warning belongs to another artifact (after a shared token change), it names that artifact. Call `design_guide` with topic `glossary` for definitions of all design terms used in the guides.
- **Paper diagrams (`paper-diagram`)**: architecture, pipeline, and method-overview figures use `Canvas` at print size, with no title inside the figure, typed shapes (such as trapezoid encoders, cylinders, and `circle_plus` residual joins), orthogonal arrows, and `N×` repeat frames. Call `design_guide` with topic `paper-diagram-example` to get a complete Transformer encoder figure as the operations for one `changes_apply`.
- **Diagram shapes**: add diagram shapes as `path` nodes (`changes_apply`, `add_node`) using recipes from `designllm://shapes` or `design_guide` topic `shapes-json` (encoder and decoder trapezoids, data parallelogram, store cylinder, add circle with `+`, multiply circle with `×`, branch diamond, frozen module hexagon, tensor cuboid). The `add.shape` GUI command performs the same operation via `ui_command_run`.
- **Paper figures (`paper-figure`)**: Data plots use the `Figure` artifact with Okabe-Ito colours and axis labels that include units. A diagram inside a `Figure` supports only rectangles, ellipses, math formulas, and straight connectors from node centre to node centre; draw elbows, residual bypasses, and typed shapes on a `Canvas`. A Figure output profile (`single_column`, `one_half_column`, `double_column`, `high_res_print`, or `presentation_16_9`, chosen with the `figure.output_profile` GUI command, also through `ui_command_run`) assigns the first colour of its palette to every plot panel, so the same series has the same colour in every panel. Later series follow the palette order (Okabe-Ito, with `high_res_print` and `presentation_16_9` starting with near-black), and each swatch in a common legend is recoloured to match its series when the series it stands for is unambiguous. A `Figure` without a chosen profile keeps its earlier colours, except when a plot has two or more series and either its panel colour is in an output profile palette or a later series repeated the panel colour; that plot gets the series colours of the current rule, and its visual approval must be given again.
- **Anti-generic design rules**: let layout explain structure and omit auxiliary labels. Do not place a coloured bar along any one edge of a card or box (top, bottom, left, or right) unless the user asks for it (`ACCENT_BAR`). Avoid stock framework and assistant colours—Tailwind blue/indigo/emerald/red, Bootstrap, Material, Google blue, ChatGPT green, Claude orange, Gemini violet, and dark navy surfaces (`COLOR_STOCK_DEFAULT`). `ICON_EMOJI` also reports a check, cross, star, sparkle, or arrow glyph (`✓ ✗ ★ ✦ →`) followed by a space and text, used in place of an icon. Additional non-blocking warnings: `SHADOW_HARD_OFFSET` (a shadow with no blur cast 2 px or more away, at least 0.25 opaque: the neobrutalist block shadow), `SHADOW_GLOW` (a coloured shadow with no offset, a coloured halo), `CARD_GHOST` (an outline of 2 px or less under a shadow blurred 16 px or more; show elevation once—new shapes carry a 1 pt grey outline by default, so set `stroke_width` to 0 on a card that keeps its shadow), `CARD_NESTED` (a shadowed card inside a larger shadowed card), `TEXT_EYEBROW` (a short capitals or letter-spaced label right above a heading), and `TEXT_SECTION_NUMBER` (decorative `01`, `02`, `03` on one page). Built-in design guidance also tells agents to avoid, where the request allows, a big-number hero metric, monospace type as a costume for text that is not code or data, Impact or Arial Black as the display face, stripe or grid textures with no subject behind them, and invented numbers; to show elevation once; and to choose a light or dark page from where the work is used. The user's request wins over these defaults. These rules paraphrase the anti-patterns of the open-source design skill impeccable (https://github.com/pbakaus/impeccable, Apache-2.0). Decks are always checked; a `Canvas` gets every design-quality warning only once it uses a v1.2 value such as a corner radius, opacity, a shadow, a gradient, or a path, keeping older `Canvas` files' validation output unchanged. Built-in `Canvas` templates use no stock framework colours, and the built-in landing page template's feature cards have no shadow under their outline.
- **Window visibility**: `--mcp` opens the desktop window on the first tool call so the user can watch. Pass `background: true` on a call, or start the server with `--background`, to run without opening the window.
- **Update checks**: on `--mcp`, a background check asks the GitHub releases API for the latest release at most once every 24 hours and caches the answer in `DesignLLMTool/update-check.json` inside the per-user app data folder (`%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS, `$XDG_DATA_HOME` or `~/.local/share` on Linux). It never delays the MCP handshake or tool calls, and a failed request is silent. Set `DESIGNLLM_NO_UPDATE_CHECK=1` to turn it off. When a newer release exists, the first successful tool result on a connection carries, once per connection, a notice naming both versions and how to update, plus `structuredContent.update_available` (`latest_version`, `current_version`, `url`, `how_to_update`).
- **Windows-only features** (not available on macOS or Linux): web-code preview window (requires WebView2; export the web code as `HTML` or `ZIP` and open it in a browser instead), automatic video poster extraction (requires WebView2), and opening a PDF as an editable `DOCX` copy (requires Microsoft Word COM automation). All other artifact kinds, editing, rendering, and exports (`PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, `ZIP`, `DOCX`, `HWPX`, `HWP`) work the same on every platform.

## Developing this repository

Build and test with Rust toolchain `1.93.1`:

```powershell
rustup toolchain install 1.93.1
cargo +1.93.1 build --release -p designllm-application
cargo +1.93.1 test -p designllm-bridge -p designllm-contracts -p designllm-canvas -p designllm-runtimes
```

- Release output path: `target\release\DesignLLMTool.exe` on Windows; `target/release/DesignLLMTool` on macOS and Linux.
- On macOS and Linux, the executable requires `assets/fonts` next to it (`./install.sh --build`, or `./install.sh` when the download fails, lays out `target/package/DesignLLMTool-0.1.2-mcp-<os>-<arch>/` with `DesignLLMTool` and `assets/fonts`; downloaded release packages unpack into `release/DesignLLMTool-0.1.2-mcp-<os>-<arch>/`).
- To update the Windows packaged build, copy `target\release\DesignLLMTool.exe` over `release\DesignLLMTool-0.1.2-mcp-win-x64\DesignLLMTool.exe`.
- Windows-only code lives behind `cfg(windows)` with Unix counterparts (`pipe_unix.rs` in `designllm-bridge`, `unix.rs` process groups in `designllm-runtimes`, exposed as `designllm_runtimes::job`).
- CI workflow (`.github/workflows/build.yml`) builds and tests on Windows, macOS (`arm64` and `x64`), and Linux (`x64` and `arm64`), uploads `DesignLLMTool-<version>-mcp-<platform>.tar.gz` packages as workflow artifacts, writes the `.sha256` file, and attaches both files to the GitHub release when a `v*` tag is pushed. Workflow `.github/workflows/install-check.yml` (run by hand, or when a release is published) installs from a fresh clone on macOS (`arm64`, `x64`) and Linux (`x64`, `arm64`) with `cargo` and `rustup` removed from `PATH`, checks that `install.sh` unpacked the release package and built nothing, and runs an MCP session against it with `DESIGNLLM_NO_UPDATE_CHECK=1`.
- Third-party licences and notices live in `release\DesignLLMTool-0.1.2-mcp-win-x64\THIRD_PARTY_LICENSES.txt`, `release\DesignLLMTool-0.1.2-mcp-win-x64\THIRD_PARTY_NOTICES.txt`, and `assets\fonts\OFL-Pretendard.txt`.
