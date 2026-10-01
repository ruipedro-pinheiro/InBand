param(
  [string]$SourceDir = (Get-Location).Path,
  [string]$ChannelDir = (Join-Path $env:LOCALAPPDATA "inband-channel"),
  [string]$BridgeUrl = "http://127.0.0.1:7447",
  [Parameter(Mandatory = $true)]
  [string]$Mailbox
)

$ErrorActionPreference = "Stop"

if (-not (Get-Command bun -ErrorAction SilentlyContinue)) {
  throw "bun is required. Install it first, then re-run this script."
}

$source = Resolve-Path $SourceDir
$srcDir = Join-Path $source "src"
foreach ($path in @(
  (Join-Path $srcDir "auth.ts"),
  (Join-Path $srcDir "channel-shim.ts"),
  (Join-Path $srcDir "channel-config.ts"),
  (Join-Path $srcDir "config.ts"),
  (Join-Path $srcDir "token-env.ts"),
  (Join-Path $source "package.json"),
  (Join-Path $source "bun.lock")
)) {
  if (-not (Test-Path $path)) { throw "Missing required source file: $path" }
}

New-Item -ItemType Directory -Force -Path (Join-Path $ChannelDir "src") | Out-Null
Copy-Item (Join-Path $srcDir "auth.ts") (Join-Path $ChannelDir "src/auth.ts") -Force
Copy-Item (Join-Path $srcDir "channel-shim.ts") (Join-Path $ChannelDir "src/channel-shim.ts") -Force
Copy-Item (Join-Path $srcDir "channel-config.ts") (Join-Path $ChannelDir "src/channel-config.ts") -Force
Copy-Item (Join-Path $srcDir "config.ts") (Join-Path $ChannelDir "src/config.ts") -Force
Copy-Item (Join-Path $srcDir "token-env.ts") (Join-Path $ChannelDir "src/token-env.ts") -Force
Copy-Item (Join-Path $source "package.json") (Join-Path $ChannelDir "package.json") -Force
Copy-Item (Join-Path $source "bun.lock") (Join-Path $ChannelDir "bun.lock") -Force

Push-Location $ChannelDir
try {
  bun install --production --frozen-lockfile
} finally {
  Pop-Location
}

$shimPath = (Join-Path $ChannelDir "src/channel-shim.ts") -replace '\\', '/'
$entry = [ordered]@{
  mcpServers = [ordered]@{
    "inband-channel" = [ordered]@{
      command = "bun"
      args = @("run", $shimPath)
      env = [ordered]@{
        INBAND_URL = $BridgeUrl
        INBAND_MAILBOX = $Mailbox
        INBAND_CLIENT_ID = "claude"
      }
    }
  }
}

Write-Host "Installed inband channel files in: $ChannelDir"
Write-Host "Add this .claude.json MCP entry manually; this script does not edit ~/.claude.json."
Write-Host "If auth is enabled, set INBAND_CLAUDE_TOKEN or INBAND_TOKEN in your user environment. This script never prints token values."
$entry | ConvertTo-Json -Depth 8
