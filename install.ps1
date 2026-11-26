<#
.SYNOPSIS
    Registers DesignLLMTool as an MCP server for the AI coding clients installed on this PC.

.DESCRIPTION
    Uses release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe in this repository, checks that it starts, and registers
    it as the MCP server "designllm" with Claude Code, Codex CLI and AGY when they are installed. It never removes or
    changes an existing registration: a client that already has a "designllm" server keeps it; to register this copy,
    change or remove that entry where it is defined (user or project settings), then run the script again. After each
    registration it reads the entry back and counts it only when its command is this copy's executable. For Gemini CLI
    and other clients (Claude Desktop, Cursor, VS Code) it prints the configuration to paste, and for Codex CLI behind a
    batch shim with a cmd.exe character in a path, a config.toml entry.

    Windows 10/11 x64 only.

.PARAMETER Project
    Optional existing folder. The workspace is kept in <Project>\.designllm and saved files go to <Project>\artifacts.
    Without it the workspace is %LOCALAPPDATA%\DesignLLMTool\workspaces\default.

.PARAMETER Client
    auto (every installed client), claude, codex, gemini, agy, or print (only print the configuration). gemini only prints
    the entry to add by hand.

.PARAMETER Background
    Register the server with --background: tools run without opening the desktop window.

.PARAMETER DryRun
    Print the commands without running them (nothing is started or changed).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\install.ps1
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\install.ps1 -Client claude -Project "D:\papers\my-paper"
#>
param(
    [string]$Project = "",
    [ValidateSet("auto", "claude", "codex", "gemini", "agy", "print")]
    [string]$Client = "auto",
    [switch]$Background,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$ServerName = "designllm"

if ($env:OS -ne "Windows_NT" -or -not [Environment]::Is64BitOperatingSystem) {
    throw "DesignLLMTool runs on 64-bit Windows only."
}

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
# The documented package first; any other build under release only when that one is missing.
$packaged = Join-Path $root "release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe"
if (Test-Path -LiteralPath $packaged) {
    $exePath = $packaged
} else {
    $exe = Get-ChildItem -Path (Join-Path $root "release") -Recurse -Filter "DesignLLMTool.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName | Select-Object -First 1
    if (-not $exe) {
        throw "release\DesignLLMTool-0.1.0-mcp-win-x64\DesignLLMTool.exe was not found. Clone the whole repository: git clone https://github.com/KIMY0UNGGON/DesignLLMTool.git"
    }
    $exePath = $exe.FullName
}

$preview = $DryRun -or $Client -eq "print"
if ($preview) {
    Write-Host "Using $exePath"
} else {
    # The executable is a windowed app: run it with Start-Process so the version check waits and its output is captured.
    $versionFile = Join-Path ([System.IO.Path]::GetTempPath()) ("designllm-version-" + [guid]::NewGuid() + ".txt")
    $probe = Start-Process -FilePath $exePath -ArgumentList "--version" -NoNewWindow -Wait -PassThru -RedirectStandardOutput $versionFile
    $version = (Get-Content -LiteralPath $versionFile -ErrorAction SilentlyContinue | Select-Object -First 1)
    Remove-Item -LiteralPath $versionFile -ErrorAction SilentlyContinue
    if ($probe.ExitCode -ne 0 -or -not $version) { throw "DesignLLMTool.exe did not start (exit code $($probe.ExitCode))." }
    Write-Host "Found $version at $exePath"
}

$serverArgs = @("--mcp")
if ($Background) { $serverArgs += "--background" }
if ($Project) {
    if (-not (Test-Path -LiteralPath $Project -PathType Container)) { throw "-Project must be an existing folder: $Project" }
    $serverArgs += @("--project", (Resolve-Path -LiteralPath $Project).Path)
}

# Native executables and batch shims only: Windows PowerShell drops a bare -- when it calls a .ps1 shim.
function Find-Cli([string]$name) {
    foreach ($candidate in @("$name.exe", "$name.cmd", "$name.bat")) {
        $command = Get-Command $candidate -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($command) { return $command.Source }
    }
    if ($name -eq "agy") {
        $agy = Join-Path $env:LOCALAPPDATA "agy\bin\agy.exe"
        if (Test-Path -LiteralPath $agy) { return $agy }
    }
    return $null
}

# A command line that pastes into PowerShell unchanged: a word with a space or a character PowerShell would read is put in
# single quotes (which expand nothing), and a quoted program path gets the call operator.
function Format-Command([string]$cli, [string[]]$arguments) {
    # PowerShell also reads the typographic quotes U+2018 to U+201E as quotes; its own escaper doubles the single ones. They
    # are built from their code points because Windows PowerShell reads this file in the ANSI code page.
    $special = '[\s''"`$&|;<>(){}@#,' + (-join [char[]](0x2018, 0x2019, 0x201A, 0x201B, 0x201C, 0x201D, 0x201E)) + ']'
    $quote = {
        param($text)
        if ($text -eq "" -or $text -match $special) {
            "'" + [System.Management.Automation.Language.CodeGeneration]::EscapeSingleQuotedStringContent($text) + "'"
        } else { $text }
    }
    $shown = ($arguments | ForEach-Object { & $quote $_ }) -join ' '
    $program = & $quote $cli
    if ($program -ne $cli) { $program = "& $program" }
    return "$program $shown"
}

# Client output without terminal colour codes (Gemini CLI colours its status marks).
function Remove-Ansi([string]$text) {
    return ($text -replace "$([char]27)\[[0-9;?]*[ -/]*[@-~]", "")
}

# Runs a native command without letting its stderr stop the script (Windows PowerShell turns redirected native stderr into
# errors under -ErrorAction Stop), reading its output as UTF-8: returns its exit code and its combined output as text,
# without terminal colour codes.
function Invoke-Native([string]$cli, [string[]]$arguments) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $encoding = $null
    try { $encoding = [Console]::OutputEncoding; [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { $encoding = $null }
    try {
        $output = & $cli @arguments 2>&1 | ForEach-Object { "$_" }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
        if ($encoding) { try { [Console]::OutputEncoding = $encoding } catch { } }
    }
    return [pscustomobject]@{ Code = $code; Output = (Remove-Ansi (@($output) -join "`n")) }
}

# Runs one client command that adds the server. The client's output goes to the console, never into the return value, so
# the function returns exactly one Boolean.
function Invoke-Cli([string]$cli, [string[]]$arguments) {
    Write-Host ("> " + (Format-Command $cli $arguments))
    if ($DryRun) { return $true }
    $result = Invoke-Native $cli $arguments
    if ($result.Output) { Write-Host $result.Output }
    return ($result.Code -eq 0)
}

# Whether the client has a server named exactly $server: State is "present", "absent", or "failed" when the lookup gave no
# clear answer (then nothing is changed); Text is what the client reported for a present entry, kept only to check which
# executable it runs and never printed, as it can hold environment values.
function Get-Registration([string]$name, [string]$cli, [string]$server) {
    $answer = { param($state, $text) [pscustomobject]@{ State = $state; Text = $text } }
    if ($name -eq "claude" -or $name -eq "codex") {
        # `mcp get` reports the entry that applies here, whatever its scope (user, project or local).
        $result = Invoke-Native $cli @("mcp", "get", $server)
        if ($result.Code -eq 0) { return (& $answer "present" $result.Output) }
        if ($result.Output -match "No MCP server named") { return (& $answer "absent" "") }
        return (& $answer "failed" "")
    }
    # AGY prints a table: a "NAME  TYPE  STATUS ..." header, then one row per server ("<name>  stdio|sse|http  enabled|disabled
    # ..."). Only that whole form is read (a header, and nothing but server rows under it); names are case-sensitive.
    $result = Invoke-Native $cli @("mcp", "list")
    if ($result.Code -ne 0) { return (& $answer "failed" "") }
    $lines = @($result.Output -split "`r?`n" | Where-Object { $_.Trim() -ne "" })
    # With no server at all, AGY prints exactly this line instead of the table.
    if ($lines.Count -eq 1 -and [string]::Equals($lines[0].Trim(), "No MCP servers configured.", [System.StringComparison]::Ordinal)) { return (& $answer "absent" "") }
    if ($lines.Count -eq 0 -or $lines[0] -notmatch '^NAME\s+TYPE\s+STATUS(\s|$)') { return (& $answer "failed" "") }
    $rows = @($lines | Select-Object -Skip 1)
    if (@($rows | Where-Object { $_ -notmatch '^\S+\s+(stdio|sse|http)\s+(enabled|disabled)(\s|$)' }).Count -gt 0) { return (& $answer "failed" "") }
    $row = $rows | Where-Object { [string]::Equals(($_ -split '\s+')[0], $server, [System.StringComparison]::Ordinal) } | Select-Object -First 1
    if ($row) { return (& $answer "present" $row) }
    return (& $answer "absent" "")
}

# Whether a registration's command is this installer's executable (not merely a path somewhere in its arguments): the
# "Command:" line of `mcp get` (Claude Code, Codex CLI), or the command column of an AGY row, which follows the status.
function Test-RunsThisCopy($registration) {
    if ($registration.State -ne "present") { return $false }
    # Both forms show the arguments joined by spaces: the stored command (a Windows path, compared without regard to case)
    # and the arguments (compared exactly, as DesignLLMTool.exe reads its flags) must be what was registered.
    $arguments = $serverArgs -join ' '
    $samePath = { param($a) $a.Trim().Trim('"').Equals($exePath, [System.StringComparison]::OrdinalIgnoreCase) }
    $commandLine = [regex]::Match($registration.Text, '(?im)^\s*command:\s*(.+?)\s*$')
    # The arguments run from after "args: " to the end of the line, kept as they are: a stored argument with a trailing
    # space is a different argument.
    $argsLine = [regex]::Match($registration.Text, '(?im)^\s*args:[ ]?([^\r\n]*)')
    if ($commandLine.Success) {
        # Ordinal: -ceq would treat strings that differ by an ignorable character (a soft hyphen, say) as equal.
        return ((& $samePath $commandLine.Groups[1].Value) -and $argsLine.Success -and [string]::Equals($argsLine.Groups[1].Value, $arguments, [System.StringComparison]::Ordinal))
    }
    # An AGY row: "<name>  <type>  <status>  <command> <arguments>". The whole remainder must be this command and these
    # arguments, so a different executable whose name merely starts with this path does not count.
    $row = [regex]::Match($registration.Text, '^\S+\s+(stdio|sse|http)\s+(enabled|disabled)\s+(.+?)\s*$')
    if ($row.Success) {
        $rest = $row.Groups[3].Value
        if ($rest.Length -le $exePath.Length -or -not (& $samePath $rest.Substring(0, $exePath.Length))) { return $false }
        return [string]::Equals($rest.Substring($exePath.Length), " $arguments", [System.StringComparison]::Ordinal)
    }
    return $false
}

$registrations = [ordered]@{
    claude = @{ Add = @("mcp", "add", "--scope", "user", $ServerName, "--", $exePath) + $serverArgs }
    codex  = @{ Add = @("mcp", "add", $ServerName, "--", $exePath) + $serverArgs }
    # Gemini CLI is not registered automatically: its `mcp add` rewrites the whole settings file (other servers' entries
    # included), replaces an existing key, and can report success when the file could not be written. The entry to add by
    # hand is printed below.
    gemini = $null
    agy    = @{ Add = @("mcp", "add", $ServerName, "--", $exePath) + $serverArgs }
}

$done = @(); $kept = @(); $failed = @(); $manual = @(); $codexToml = $false
if ($Client -ne "print") {
    $targets = if ($Client -eq "auto") { $registrations.Keys } else { @($Client) }
    foreach ($name in $targets) {
        $cli = Find-Cli $name
        if (-not $cli) {
            if ($Client -ne "auto") { Write-Warning "$name was not found on PATH."; $failed += $name }
            continue
        }
        $steps = $registrations[$name]
        if (-not $steps) {
            $manual += $name
            continue
        }
        # cmd.exe expands % and ! and treats & | < > ^ as syntax in what a batch shim (.cmd or .bat) runs and forwards: such a
        # path would be registered wrongly, so this client is left to the JSON configuration printed below.
        if (($cli -like "*.cmd" -or $cli -like "*.bat") -and (@($cli, $exePath) + $serverArgs | Where-Object { $_ -match '[%!^&|<>"]' })) {
            Write-Warning "$name runs through a batch shim and a path contains one of % ! ^ & | < > `". It was not registered; add the configuration printed below to its settings instead."
            if ($name -eq "codex") { $codexToml = $true }
            $failed += $name
            continue
        }
        if (-not $DryRun) {
            $current = Get-Registration $name $cli $ServerName
            if ($current.State -eq "failed") {
                Write-Warning "$name did not report clearly whether it has a '$ServerName' server, so nothing was changed for it. Check with: $(Format-Command $cli @('mcp', 'list'))"
                $failed += $name
                continue
            }
            if ($current.State -eq "present") {
                # An existing registration is never removed or changed here: it may hold settings this script cannot restore,
                # and it may come from a project's settings rather than the user's.
                if (Test-RunsThisCopy $current) {
                    Write-Host "$name already has a '$ServerName' server that runs this copy; kept as it is."
                } else {
                    Write-Host "$name already has a '$ServerName' server; kept as it is. To point it at this copy, change or remove that entry in the $name settings where it is defined, then run install.ps1 again."
                }
                $kept += $name
                continue
            }
        }
        $added = Invoke-Cli $cli $steps.Add
        # A client can report success without having saved the entry, or another entry can have taken the name meanwhile:
        # read it back and check that it runs this copy.
        if ($added -and -not $DryRun) { $added = Test-RunsThisCopy (Get-Registration $name $cli $ServerName) }
        if ($added) {
            $done += $name
            # The clients print arguments joined by spaces, so a project path with a space cannot be told from two
            # arguments there.
            if (-not $DryRun -and ($serverArgs | Where-Object { $_ -match '\s' })) {
                Write-Warning "$name shows the arguments joined by spaces, so this script could not check that the project path stayed one argument. If the server does not start, register it by hand with the JSON configuration printed below."
            }
        } else {
            Write-Warning "$name did not register '$ServerName' for this copy. Check its settings, or register it by hand: $(Format-Command $cli $steps.Add)"
            $failed += $name
        }
    }
}

$entry = [ordered]@{ command = $exePath; args = $serverArgs }
Write-Host ""
$geminiHome = if ($env:GEMINI_CLI_HOME) { $env:GEMINI_CLI_HOME } else { $env:USERPROFILE }
Write-Host ("Claude Desktop, Cursor or Gemini CLI: add this to the MCP configuration (mcpServers); for Gemini CLI that is {0}, unless 'gemini mcp list' already shows a '{1}' server (an extension can provide one):" -f (Join-Path $geminiHome ".gemini\settings.json"), $ServerName)
Write-Host (([ordered]@{ mcpServers = [ordered]@{ $ServerName = $entry } }) | ConvertTo-Json -Depth 5)
Write-Host ""
if ($codexToml) {
    # Codex CLI keeps its servers in TOML (%USERPROFILE%\.codex\config.toml, or config.toml in CODEX_HOME).
    $toml = { param($text) '"' + ($text -replace '\\', '\\' -replace '"', '\"') + '"' }
    $codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE ".codex" }
    Write-Host ("Codex CLI: add this to {0}:" -f (Join-Path $codexHome "config.toml"))
    Write-Host ("[mcp_servers.{0}]" -f $ServerName)
    Write-Host ("command = " + (& $toml $exePath))
    Write-Host ("args = [" + (($serverArgs | ForEach-Object { & $toml $_ }) -join ", ") + "]")
    Write-Host ""
}
Write-Host "VS Code: add this to .vscode\mcp.json (servers):"
$vscodeEntry = [ordered]@{ type = "stdio"; command = $exePath; args = $serverArgs }
Write-Host (([ordered]@{ servers = [ordered]@{ $ServerName = $vscodeEntry } }) | ConvertTo-Json -Depth 5)
Write-Host ""
if ($DryRun) {
    if ($done.Count -gt 0) { Write-Host ("Dry run: would register '{0}' with: {1} (each one only if it has no '{0}' server yet)." -f $ServerName, ($done -join ", ")) }
} else {
    if ($done.Count -gt 0) {
        Write-Host ("Registered '{0}' with: {1}. Restart the client, then ask it to call the design_guide tool to check the connection." -f $ServerName, ($done -join ", "))
    }
    if ($kept.Count -gt 0) { Write-Host ("Kept the existing '{0}' server of: {1}." -f $ServerName, ($kept -join ", ")) }
}
if ($manual.Count -gt 0) {
    Write-Host ("Add '{0}' by hand to: {1} (the mcpServers entry above)." -f $ServerName, ($manual -join ", "))
}
if (-not $DryRun) {
    if ($Client -ne "print" -and $done.Count -eq 0 -and $kept.Count -eq 0 -and $failed.Count -eq 0 -and $manual.Count -eq 0) {
        Write-Host "No client was registered. Install Claude Code, Codex CLI, Gemini CLI or AGY, or use the configuration above."
    }
}
if ($failed.Count -gt 0) {
    Write-Warning ("Not registered: {0}." -f ($failed -join ", "))
    exit 1
}
