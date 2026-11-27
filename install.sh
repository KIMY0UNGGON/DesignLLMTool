#!/usr/bin/env bash
# Registers DesignLLMTool as an MCP server for the AI coding clients installed on this Mac or Linux PC.
#
# Uses release/DesignLLMTool-<version>-mcp-<os>-<arch>/DesignLLMTool when this repository carries a package for this
# platform. Otherwise it builds one from source (Rust toolchain 1.93.1 through rustup, or the cargo on PATH) into
# target/package/DesignLLMTool-<version>-mcp-<os>-<arch>/. It checks that the executable starts and registers it as the
# MCP server "designllm" with Claude Code, Codex CLI and AGY when they are installed. It never removes or changes an
# existing registration: a client that already has a "designllm" server keeps it; to register this copy, change or remove
# that entry where it is defined (user or project settings), then run the script again. After each registration it reads
# the entry back and counts it only when its command is this copy's executable. For Gemini CLI and other clients (Claude
# Desktop, Cursor, VS Code) it prints the configuration to paste.
#
# Options:
#   --project DIR   Existing folder. The workspace is kept in DIR/.designllm and saved files go to DIR/artifacts. Without
#                   it the workspace is the folder in DESIGNLLM_WORKSPACE, or else the per-user application data folder
#                   (~/Library/Application Support/DesignLLMTool/workspaces/default on macOS,
#                   ${XDG_DATA_HOME:-~/.local/share}/DesignLLMTool/workspaces/default on Linux).
#   --client NAME   auto (every installed client), claude, codex, gemini, agy, or print (only print the configuration).
#                   gemini only prints the entry to add by hand.
#   --background    Register the server with --background: tools run without opening the desktop window.
#   --build         Build from source even when a package exists (also with --client print).
#   --dry-run       Print the commands without running them (nothing is built, started or changed).
#
# Examples:
#   ./install.sh
#   ./install.sh --client claude --project ~/papers/my-paper

set -euo pipefail

SERVER_NAME="designllm"
PROJECT=""
CLIENT="auto"
BACKGROUND=0
DRY_RUN=0
FORCE_BUILD=0

die() { echo "error: $*" >&2; exit 1; }
warn() { echo "warning: $*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --project) [ $# -ge 2 ] || die "--project needs a folder"; PROJECT="$2"; shift 2 ;;
        --client) [ $# -ge 2 ] || die "--client needs a name"; CLIENT="$2"; shift 2 ;;
        --background) BACKGROUND=1; shift ;;
        --build) FORCE_BUILD=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done
case "$CLIENT" in auto|claude|codex|gemini|agy|print) ;; *) die "--client must be auto, claude, codex, gemini, agy or print" ;; esac

case "$(uname -s)" in
    Darwin) OS="macos" ;;
    Linux) OS="linux" ;;
    *) die "this script is for macOS and Linux; on Windows run install.ps1" ;;
esac
case "$(uname -m)" in
    x86_64|amd64) ARCH="x64" ;;
    arm64|aarch64) ARCH="arm64" ;;
    *) die "unsupported processor: $(uname -m)" ;;
esac

ROOT="$(cd "$(dirname "$0")" && pwd)"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[ -n "$VERSION" ] || die "cannot read the version from Cargo.toml"
PACKAGE_NAME="DesignLLMTool-$VERSION-mcp-$OS-$ARCH"
PACKAGED="$ROOT/release/$PACKAGE_NAME/DesignLLMTool"
LOCAL_PACKAGE="$ROOT/target/package/$PACKAGE_NAME"
PREVIEW=0
if [ "$DRY_RUN" = 1 ] || [ "$CLIENT" = "print" ]; then PREVIEW=1; fi

# Builds the application and lays out a package next to it: the executable and the fonts it loads at run time.
build_package() {
    command -v cargo >/dev/null 2>&1 || die "no package for $OS-$ARCH in release/ and no cargo on PATH. Install Rust (https://rustup.rs), then run this script again."
    local toolchain=()
    if command -v rustup >/dev/null 2>&1; then
        if ! rustup toolchain list | grep -q '^1\.93\.1'; then
            echo "Installing Rust toolchain 1.93.1..."
            rustup toolchain install 1.93.1 --profile minimal
        fi
        toolchain=(+1.93.1)
    fi
    echo "Building DesignLLMTool $VERSION for $OS-$ARCH (this takes several minutes the first time)..."
    (cd "$ROOT" && cargo "${toolchain[@]}" build --release -p designllm-application)
    rm -rf "$LOCAL_PACKAGE"
    mkdir -p "$LOCAL_PACKAGE/assets"
    cp "$ROOT/target/release/DesignLLMTool" "$LOCAL_PACKAGE/DesignLLMTool"
    cp -R "$ROOT/assets/fonts" "$LOCAL_PACKAGE/assets/fonts"
    local notices="$ROOT/release/DesignLLMTool-$VERSION-mcp-win-x64"
    for file in THIRD_PARTY_LICENSES.txt THIRD_PARTY_NOTICES.txt; do
        if [ -f "$notices/$file" ]; then cp "$notices/$file" "$LOCAL_PACKAGE/$file"; fi
    done
}

if [ "$FORCE_BUILD" = 0 ] && [ -x "$PACKAGED" ]; then
    EXE="$PACKAGED"
elif [ "$FORCE_BUILD" = 0 ] && [ -x "$LOCAL_PACKAGE/DesignLLMTool" ]; then
    EXE="$LOCAL_PACKAGE/DesignLLMTool"
elif [ "$DRY_RUN" = 1 ] || { [ "$PREVIEW" = 1 ] && [ "$FORCE_BUILD" = 0 ]; }; then
    EXE="$LOCAL_PACKAGE/DesignLLMTool"
    echo "$EXE is not built yet; run ./install.sh (or add --build) to build it."
else
    build_package
    EXE="$LOCAL_PACKAGE/DesignLLMTool"
fi

if [ "$PREVIEW" = 1 ]; then
    echo "Using $EXE"
else
    VERSION_TEXT="$("$EXE" --version 2>/dev/null | head -n 1 || true)"
    [ -n "$VERSION_TEXT" ] || die "$EXE did not start."
    echo "Found $VERSION_TEXT at $EXE"
fi

SERVER_ARGS=(--mcp)
if [ "$BACKGROUND" = 1 ]; then SERVER_ARGS+=(--background); fi
if [ -n "$PROJECT" ]; then
    [ -d "$PROJECT" ] || die "--project must be an existing folder: $PROJECT"
    SERVER_ARGS+=(--project "$(cd "$PROJECT" && pwd -P)")
fi
ARGS_TEXT="${SERVER_ARGS[*]}"

if [ "$OS" = "linux" ] && [ "$BACKGROUND" = 0 ] && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
    warn "no graphical session (DISPLAY or WAYLAND_DISPLAY) is set here. The server opens the desktop window on its first tool call; on a machine without a desktop, run this script with --background."
fi

# A shell-quoted command line, for messages.
show_command() {
    local out="" word
    for word in "$@"; do
        if [[ "$word" =~ ^[A-Za-z0-9_./:=+-]+$ ]]; then out="$out $word"; else out="$out '${word//\'/\'\\\'\'}'"; fi
    done
    echo "${out# }"
}

# Client output without terminal colour codes.
strip_ansi() { sed $'s/\x1b\\[[0-9;?]*[ -/]*[@-~]//g'; }

# Sets STATE to the registration state of the server for a client: present, absent or failed. The entry text goes to
# REGISTRATION_TEXT (never printed: it can hold environment values).
STATE=""
REGISTRATION_TEXT=""
registration_state() {
    local name="$1" cli="$2" output code=0
    STATE="failed"
    REGISTRATION_TEXT=""
    if [ "$name" = "claude" ] || [ "$name" = "codex" ]; then
        output="$("$cli" mcp get "$SERVER_NAME" 2>&1 | strip_ansi)" || code=$?
        if [ "$code" = 0 ]; then REGISTRATION_TEXT="$output"; STATE=present; return; fi
        if printf '%s' "$output" | grep -q "No MCP server named"; then STATE=absent; return; fi
        return
    fi
    # AGY prints a table: a "NAME  TYPE  STATUS ..." header, then one row per server.
    output="$("$cli" mcp list 2>&1 | strip_ansi)" || code=$?
    [ "$code" = 0 ] || { return; }
    local lines
    lines="$(printf '%s\n' "$output" | sed '/^[[:space:]]*$/d')"
    if [ "$(printf '%s\n' "$lines" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" = "No MCP servers configured." ]; then STATE=absent; return; fi
    printf '%s\n' "$lines" | head -n 1 | grep -Eq '^NAME[[:space:]]+TYPE[[:space:]]+STATUS([[:space:]]|$)' || { return; }
    if printf '%s\n' "$lines" | tail -n +2 | grep -Evq '^[^[:space:]]+[[:space:]]+(stdio|sse|http)[[:space:]]+(enabled|disabled)([[:space:]]|$)'; then
        return
    fi
    local row
    row="$(printf '%s\n' "$lines" | tail -n +2 | awk -v s="$SERVER_NAME" '$1 == s { print; exit }')"
    if [ -n "$row" ]; then REGISTRATION_TEXT="$row"; STATE=present; else STATE=absent; fi
}

# Whether REGISTRATION_TEXT runs this copy with these arguments.
runs_this_copy() {
    local text="$REGISTRATION_TEXT" command args
    command="$(printf '%s\n' "$text" | sed -n 's/^[[:space:]]*[Cc]ommand:[[:space:]]*//p' | head -n 1 | sed 's/[[:space:]]*$//')"
    if [ -n "$command" ]; then
        args="$(printf '%s\n' "$text" | sed -n 's/^[[:space:]]*[Aa]rgs:[ ]\{0,1\}//p' | head -n 1)"
        command="${command#\"}"; command="${command%\"}"
        [ "$command" = "$EXE" ] || return 1
        [ "$args" = "$ARGS_TEXT" ]
        return
    fi
    local rest
    rest="$(printf '%s\n' "$text" | sed -En 's/^[^[:space:]]+[[:space:]]+(stdio|sse|http)[[:space:]]+(enabled|disabled)[[:space:]]+//p' | sed 's/[[:space:]]*$//')"
    [ "$rest" = "$EXE $ARGS_TEXT" ]
}

DONE=(); KEPT=(); FAILED=(); MANUAL=()
if [ "$CLIENT" != "print" ]; then
    if [ "$CLIENT" = "auto" ]; then TARGETS=(claude codex gemini agy); else TARGETS=("$CLIENT"); fi
    for name in "${TARGETS[@]}"; do
        cli="$(command -v "$name" 2>/dev/null || true)"
        if [ -z "$cli" ] && [ "$name" = "agy" ] && [ -x "$HOME/.local/bin/agy" ]; then cli="$HOME/.local/bin/agy"; fi
        if [ -z "$cli" ]; then
            if [ "$CLIENT" != "auto" ]; then warn "$name was not found on PATH."; FAILED+=("$name"); fi
            continue
        fi
        if [ "$name" = "gemini" ]; then
            # Gemini CLI is not registered automatically: its `mcp add` rewrites the whole settings file and can report
            # success when the file could not be written. The entry to add by hand is printed below.
            MANUAL+=("$name")
            continue
        fi
        case "$name" in
            claude) ADD=(mcp add --scope user "$SERVER_NAME" -- "$EXE" "${SERVER_ARGS[@]}") ;;
            *) ADD=(mcp add "$SERVER_NAME" -- "$EXE" "${SERVER_ARGS[@]}") ;;
        esac
        if [ "$DRY_RUN" = 0 ]; then
            registration_state "$name" "$cli"
            if [ "$STATE" = "failed" ]; then
                warn "$name did not report clearly whether it has a '$SERVER_NAME' server, so nothing was changed for it. Check with: $(show_command "$cli" mcp list)"
                FAILED+=("$name"); continue
            fi
            if [ "$STATE" = "present" ]; then
                if runs_this_copy; then
                    echo "$name already has a '$SERVER_NAME' server that runs this copy; kept as it is."
                else
                    echo "$name already has a '$SERVER_NAME' server; kept as it is. To point it at this copy, change or remove that entry in the $name settings where it is defined, then run install.sh again."
                fi
                KEPT+=("$name"); continue
            fi
        fi
        echo "> $(show_command "$cli" "${ADD[@]}")"
        if [ "$DRY_RUN" = 1 ]; then DONE+=("$name"); continue; fi
        added=0
        if "$cli" "${ADD[@]}"; then
            # Read the entry back: a client can report success without having saved it.
            registration_state "$name" "$cli"
            if [ "$STATE" = "present" ] && runs_this_copy; then added=1; fi
        fi
        if [ "$added" = 1 ]; then
            DONE+=("$name")
        else
            warn "$name did not register '$SERVER_NAME' for this copy. Check its settings, or register it by hand: $(show_command "$cli" "${ADD[@]}")"
            FAILED+=("$name")
        fi
    done
fi

json_string() {
    local text="${1//\\/\\\\}"
    text="${text//\"/\\\"}"
    printf '"%s"' "$text"
}
JSON_ARGS=""
for arg in "${SERVER_ARGS[@]}"; do JSON_ARGS="$JSON_ARGS${JSON_ARGS:+, }$(json_string "$arg")"; done

echo ""
echo "Claude Desktop, Cursor or Gemini CLI: add this to the MCP configuration (mcpServers); for Gemini CLI that is ${GEMINI_CLI_HOME:-$HOME}/.gemini/settings.json, unless 'gemini mcp list' already shows a '$SERVER_NAME' server (an extension can provide one):"
cat <<EOF
{
  "mcpServers": {
    "$SERVER_NAME": {
      "command": $(json_string "$EXE"),
      "args": [$JSON_ARGS]
    }
  }
}
EOF
echo ""
echo "VS Code: add this to .vscode/mcp.json (servers):"
cat <<EOF
{
  "servers": {
    "$SERVER_NAME": {
      "type": "stdio",
      "command": $(json_string "$EXE"),
      "args": [$JSON_ARGS]
    }
  }
}
EOF
echo ""

join() { local IFS=","; echo "$*" | sed 's/,/, /g'; }
if [ "$DRY_RUN" = 1 ]; then
    if [ ${#DONE[@]} -gt 0 ]; then echo "Dry run: would register '$SERVER_NAME' with: $(join "${DONE[@]}") (each one only if it has no '$SERVER_NAME' server yet)."; fi
else
    if [ ${#DONE[@]} -gt 0 ]; then echo "Registered '$SERVER_NAME' with: $(join "${DONE[@]}"). Restart the client, then ask it to call the design_guide tool to check the connection."; fi
    if [ ${#KEPT[@]} -gt 0 ]; then echo "Kept the existing '$SERVER_NAME' server of: $(join "${KEPT[@]}")."; fi
fi
if [ ${#MANUAL[@]} -gt 0 ]; then echo "Add '$SERVER_NAME' by hand to: $(join "${MANUAL[@]}") (the mcpServers entry above)."; fi
if [ "$DRY_RUN" = 0 ] && [ "$CLIENT" != "print" ] && [ ${#DONE[@]} -eq 0 ] && [ ${#KEPT[@]} -eq 0 ] && [ ${#FAILED[@]} -eq 0 ] && [ ${#MANUAL[@]} -eq 0 ]; then
    echo "No client was registered. Install Claude Code, Codex CLI, Gemini CLI or AGY, or use the configuration above."
fi
if [ ${#FAILED[@]} -gt 0 ]; then
    warn "Not registered: $(join "${FAILED[@]}")."
    exit 1
fi
