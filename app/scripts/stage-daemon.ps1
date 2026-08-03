
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$appRoot  = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $appRoot

Write-Host "Building vidcull-daemon (release)..."
Push-Location $repoRoot
try {
    cargo build -p vidcull-daemon --release
    if ($LASTEXITCODE -ne 0) {
        Write-Host ""
        Write-Host "Build failed. If a running daemon is locking target/release/vidcull-daemon.exe,"
        Write-Host "stop the daemon process and retry."
        throw "cargo build exited with code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

$triple = (rustc -vV | Select-String '^host:').ToString().Split(':')[1].Trim()

$dest = Join-Path $appRoot 'src-tauri/binaries'
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$isWindows    = $triple -like '*windows*'
$srcName      = if ($isWindows) { 'vidcull-daemon.exe' } else { 'vidcull-daemon' }
$sidecarName  = if ($isWindows) { "vidcull-daemon-$triple.exe" } else { "vidcull-daemon-$triple" }

$from = Join-Path $repoRoot "target/release/$srcName"
if (-not (Test-Path $from)) {
    throw "build artefact not found at $from"
}

Copy-Item $from (Join-Path $dest $sidecarName) -Force
Write-Host "Staged $sidecarName -> $dest"
Write-Host "Ensure tauri.conf.json bundle.externalBin contains `"binaries/vidcull-daemon`"."
