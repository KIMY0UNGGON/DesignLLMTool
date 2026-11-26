# AGENTS.md

Instructions for AI agents installing, connecting to, and working with DesignLLMTool.

## Install and connect

1. Verify the host meets the requirements: Windows 10 or 11 (64-bit) and Git. Optional: Microsoft Word (only for opening a PDF as an editable DOCX copy) and Microsoft Edge WebView2 Runtime (included in Windows 11, for web-code preview).
2. Clone the repository and enter the clone directory (about 90 MB; contains the ready-to-run binary `release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe`):

```powershell
git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git
cd DesignLLMTool
```

3. Optionally ask the user which existing project folder to use for the workspace. With a project folder, the workspace lives in `<folder>\.designllm` and native files are saved to `<folder>\artifacts`. Without it, the workspace is `%LOCALAPPDATA%\DesignLLMTool\workspaces\default` (or the folder in `DESIGNLLM_WORKSPACE`).
4. Run `install.ps1`. It verifies that `release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe` starts, and it registers the MCP server `designllm` with every installed client among `Claude Code`, `Codex CLI`, and `AGY`. For `Gemini CLI`, it only prints the `mcpServers` entry because `gemini mcp add` rewrites the entire settings file and can report success without saving:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

   Available installer flags:
   - `-Project "<existing folder>"`: keep the workspace in that folder.
   - `-Client claude|codex|gemini|agy|print`: target one client, or print configuration only.
   - `-Background`: tools run without opening the app window.
   - `-DryRun`: print commands without running or changing anything.

   Note: `install.ps1` never removes or changes an existing `designllm` registration. To register this copy, change or remove that existing entry in the client settings where it is defined (user or project), then run `install.ps1` again. The installer counts a registration only when reading the entry back shows this copy's `DesignLLMTool.exe`. If a client does not report its server list clearly, the installer leaves that client unchanged. When a requested registration fails, `install.ps1` exits with code `1`. When `Codex CLI` runs through a batch shim and a path contains a character that `cmd.exe` would read, the installer instead prints a `[mcp_servers.designllm]` TOML entry together with the path to the Codex settings file that the client uses (`%USERPROFILE%\.codex\config.toml`, or `config.toml` in `CODEX_HOME` when that variable is set).

5. If a CLI is missing, or when setting up `Claude Desktop`, `Cursor`, or `VS Code`, print the JSON configuration and tell the user where to paste it:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Client print
```

   - For `Gemini CLI`, `Claude Desktop`, and `Cursor`, paste the printed `mcpServers` JSON entry into the client's MCP configuration unless that client already has a `designllm` server (for `Gemini CLI`, check `gemini mcp list` because an extension can provide one). For `Gemini CLI`, use `%USERPROFILE%\.gemini\settings.json` or the settings file under `GEMINI_CLI_HOME`.
   - For `VS Code`, paste the printed `servers` JSON entry into `.vscode/mcp.json`.
   - For manual CLI registration (replacing `<repo>` with the clone folder), use `codex.cmd` in PowerShell for Codex CLI so PowerShell does not drop the bare `--`:

```powershell
claude mcp add --scope user designllm -- "<repo>\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe" --mcp
codex.cmd mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe" --mcp
agy mcp add designllm -- "<repo>\release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe" --mcp
```

6. Verify the connection: have the user restart the AI client, then call `design_guide`. If it returns the design guide index, the connection works.

## How to work with DesignLLMTool

### Standard workflow

1. Call `workspace_list`.
2. Before making the first change to an artifact, call `design_guide` with its `artifact_id`. That artifact-bound guide—or the guide attached to an `artifact_create`, `artifact_open`, or `artifact_inspect` result, or carried by a `GUIDE_REQUIRED` refusal—unlocks changes on the connection for that artifact and for every other artifact that uses the same guide (for example, every deck). Calling `design_guide` with only a topic (`paper-diagram`, `paper-diagram-example`, `paper-figure`, `slides`, `glossary`, `shapes`, `shapes-json`) is reference reading and unlocks nothing. Follow the guide; if the user's explicit instructions conflict with it, the user's instructions win.
3. Create or open the artifact (`artifact_create`, `artifact_open`), read it with `artifact_inspect`, and apply edits with `changes_validate` then `changes_apply`.
4. Read the design-quality warnings in each `changes_apply` result and fix them; inspect the rendered output with `render_get`.
5. Deliver with `artifact_export` (`PDF`, `SVG`, `PNG`, `PPTX`, `HTML`) or `artifact_save` (native file).

### MCP resources and prompts

- `designllm://manual/index`: all guides.
- `designllm://shapes`: shape recipes.
- `designllm://presets`: frame sizes.
- MCP prompts: one per task guide.

### Artifacts, guides, and quality rules

- **Artifact kinds and exports**: `Canvas` (screens, landing pages, posters, social posts, paper architecture diagrams), `Slides` (saved as PPTX), `Figure` (data plots from CSV with evidence from PDF), `DOCX`, `HWPX`, and `HWP` documents, and `Web code`. Export via `PDF`, `SVG`, `PNG`, `PPTX`, `HTML`, and `ZIP` (web code); documents save in their native format.
- **Guide enforcement**: if a change is sent to an artifact before the connection has received its guide, that first change is refused with `GUIDE_REQUIRED`, carrying the guide. After each applied change to a `Canvas` or a deck, the response lists design-quality warnings; if a warning belongs to another artifact (after a shared token change), it names that artifact. Call `design_guide` with topic `glossary` for definitions of all design terms used in the guides.
- **Paper diagrams (`paper-diagram`)**: architecture, pipeline, and method-overview figures use `Canvas` at print size, with no title inside the figure, typed shapes (such as trapezoid encoders, cylinders, and `circle_plus` residual joins), orthogonal arrows, and `N×` repeat frames. Call `design_guide` with topic `paper-diagram-example` to get a complete Transformer encoder figure as a single `changes_apply` payload.
- **Diagram shapes**: add diagram shapes as `path` nodes (`changes_apply`, `add_node`) using recipes from `designllm://shapes` or `design_guide` topic `shapes-json` (encoder and decoder trapezoids, data parallelogram, store cylinder, add circle with `+`, multiply circle with `×`, branch diamond, frozen module hexagon, tensor cuboid). The `add.shape` GUI command performs the same operation via `ui_command_run`.
- **Paper figures (`paper-figure`)**: Data plots use the `Figure` artifact with Okabe-Ito colours and axis labels that include units. A diagram inside a `Figure` supports only rectangles, ellipses, math formulas, and straight connectors from node centre to node centre; draw elbows, residual bypasses, and typed shapes on a `Canvas`. A Figure output profile (`single column`, `double column`, `print`, or `presentation`) assigns the first colour of its palette to every plot panel, so the same series has the same colour in every panel. Later series follow the palette order (Okabe-Ito, with `print` and `presentation` starting with near-black), and the swatches in a common legend are recoloured to match their series. A `Figure` without a chosen profile keeps its earlier colours, except when a plot has two or more series and either its panel colour is in an output profile palette or a later series repeated the panel colour; that plot gets the series colours of the current rule, and its visual approval must be given again.
- **Anti-generic design rules**: let layout explain structure and omit auxiliary labels. Do not place a coloured bar along the top or left edge of a card or box (the blog-callout look) unless the user asks for it (`ACCENT_BAR`). Avoid stock framework and assistant colours—Tailwind blue/indigo/emerald/red, Bootstrap, Material, Google blue, ChatGPT green, Claude orange, Gemini violet, and dark navy surfaces (`COLOR_STOCK_DEFAULT`: always checked on decks; checked on a `Canvas` once it uses a v1.2 value such as a corner radius, opacity, a shadow, a gradient, or a path, keeping older `Canvas` files' validation output unchanged). Built-in `Canvas` templates use no stock framework colours.
- **Window visibility**: `--mcp` opens the desktop window on the first tool call so the user can watch. Pass `background: true` on a call, or start the server with `--background`, to run without opening the window.

## Developing this repository

Build and test with Rust toolchain `1.93.1`:

```powershell
rustup toolchain install 1.93.1
cargo +1.93.1 build --release -p designllm-application
cargo +1.93.1 test -p designllm-bridge -p designllm-contracts -p designllm-canvas -p designllm-runtimes
```

- Release output path: `target\release\DesignLLMTool.exe`.
- To update the packaged build, copy `target\release\DesignLLMTool.exe` over `release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe`.
- Third-party licences and notices live in `release\DesignLLMTool-0.1.0-mcp-win-x64\THIRD_PARTY_LICENSES.txt`, `release\DesignLLMTool-0.1.0-mcp-win-x64\THIRD_PARTY_NOTICES.txt`, and `assets\fonts\OFL-Pretendard.txt`.
