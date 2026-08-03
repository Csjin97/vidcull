
[CmdletBinding()]
param(
    [string]$Platform = 'windows-x86_64',
    [switch]$IncludeFfmpeg
)

$ErrorActionPreference = 'Stop'
$appRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $appRoot

$triple = (rustc -vV | Select-String '^host:').ToString().Split(':')[1].Trim()
$dest = Join-Path $appRoot 'src-tauri/binaries'
New-Item -ItemType Directory -Force -Path $dest | Out-Null

if ($IncludeFfmpeg) {
    $src = Join-Path $repoRoot "vendor/ffmpeg/$Platform"
    if (-not (Test-Path $src)) {
        throw "vendored ffmpeg not found at $src — run scripts/fetch-ffmpeg.ps1 first"
    }
    $ffmpegFiles = @(
        'ffmpeg.exe', 'ffprobe.exe',
        'avcodec-61.dll', 'avformat-61.dll', 'avutil-59.dll',
        'swscale-8.dll', 'swresample-5.dll', 'avfilter-10.dll', 'avdevice-61.dll'
    )
    $ffmpegDest = Join-Path $appRoot 'src-tauri/ffmpeg-runtime'
    New-Item -ItemType Directory -Force -Path $ffmpegDest | Out-Null
    foreach ($name in $ffmpegFiles) {
        $from = Join-Path $src $name
        if (-not (Test-Path $from)) { throw "missing $from — vendor/ffmpeg incomplete (re-run scripts/fetch-ffmpeg.ps1)" }
        Copy-Item $from (Join-Path $ffmpegDest $name) -Force
        Write-Host "staged ffmpeg-runtime/$name"
    }
}

$dsExe = if ($Platform -like 'windows-*') { 'vidcull-decode-sidecar.exe' } else { 'vidcull-decode-sidecar' }
$dsFrom = Join-Path $repoRoot "crates/vidcull-decode-sidecar/target/release/$dsExe"
if (Test-Path $dsFrom) {
    $dsName = if ($Platform -like 'windows-*') { "vidcull-decode-sidecar-$triple.exe" } else { "vidcull-decode-sidecar-$triple" }
    Copy-Item $dsFrom (Join-Path $dest $dsName) -Force
    Write-Host "staged $dsName"
}
else {
    Write-Warning "decode sidecar not built ($dsFrom) — partial-clip decode acceleration absent; daemon uses per-frame ffmpeg. Build it on a machine with LGPL FFmpeg dev libs to enable it."
}

Write-Host "Sidecars staged in $dest."
