
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$appRoot  = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $appRoot

$env:RUSTFLAGS = @(
    "--remap-path-prefix=$env:USERPROFILE=/home"
    "--remap-path-prefix=$repoRoot=/vidcull"
    "--remap-path-prefix=$env:USERPROFILE\.cargo=/cargo"
    "--remap-path-prefix=$env:USERPROFILE\.rustup=/rustup"
) -join ' '

Write-Host "RUSTFLAGS set (path remap active)."


Write-Host "Staging daemon sidecar..."
& "$PSScriptRoot\stage-daemon.ps1"
if ($LASTEXITCODE -ne 0) {
    throw "stage-daemon.ps1 exited with code $LASTEXITCODE"
}

Write-Host "Verifying bundled daemon freshness..."
& node "$PSScriptRoot\verify-build-freshness.mjs"
if ($LASTEXITCODE -ne 0) {
    throw "build-freshness guard failed (stale/missing daemon). See message above; re-stage the daemon."
}

Write-Host "Staging decode sidecar + ffmpeg/libav bundle..."
& "$PSScriptRoot\stage-sidecars.ps1" -IncludeFfmpeg
if ($LASTEXITCODE -ne 0) {
    throw "stage-sidecars.ps1 exited with code $LASTEXITCODE"
}

$triple = (rustc -vV | Select-String '^host:').ToString().Split(':')[1].Trim()
$dsStaged = Join-Path $appRoot "src-tauri\binaries\vidcull-decode-sidecar-$triple.exe"
if (-not (Test-Path $dsStaged)) {
    throw "decode sidecar not staged ($dsStaged). tauri externalBin requires it — build it first on a packaging machine with LGPL FFmpeg dev libs (FFMPEG_DIR) + the same RUSTFLAGS remap as this script: cargo build --release --manifest-path crates/vidcull-decode-sidecar/Cargo.toml"
}

Write-Host "Building Tauri app (release)..."
Push-Location $appRoot
try {
    npm install
    if ($LASTEXITCODE -ne 0) { throw "npm install exited with code $LASTEXITCODE" }

    npm run tauri build
    if ($LASTEXITCODE -ne 0) { throw "npm run tauri build exited with code $LASTEXITCODE" }
}
finally {
    Pop-Location
}

$bundleDir = Join-Path $appRoot 'src-tauri\target\release\bundle\nsis'
Write-Host ""
Write-Host "Build complete.  NSIS installers:"
if (Test-Path $bundleDir) {
    Get-ChildItem -Path $bundleDir -Filter '*-setup.exe' |
        ForEach-Object { Write-Host "  $($_.FullName)" }
} else {
    Write-Host "  (bundle dir not found: $bundleDir)"
    Write-Host "  Check src-tauri/tauri.conf.json bundle.targets includes 'nsis'."
}
