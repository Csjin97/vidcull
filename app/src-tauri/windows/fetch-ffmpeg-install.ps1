
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Dest
)

$ErrorActionPreference = 'Stop'

$url = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-06-17-14-17/ffmpeg-n7.1.4-39-ga5faeca88f-win64-lgpl-shared-7.1.zip'
$sha = '8D242E72FADF5838A1CCAE4C7655649CAE02A98476DCF50C959D1F19BDA19FBF'

$needExe = @('ffmpeg.exe', 'ffprobe.exe')

if (-not (Test-Path $Dest)) {
    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
}

$haveFfmpeg = Test-Path (Join-Path $Dest 'ffmpeg.exe')
$haveDll = @(Get-ChildItem -Path $Dest -Filter '*.dll' -ErrorAction SilentlyContinue).Count -gt 0
if ($haveFfmpeg -and $haveDll) {
    Write-Host "ffmpeg + libav DLLs already present in $Dest — skipping download."
    exit 0
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("vidcull_ffmpeg_" + [guid]::NewGuid().ToString('N') + '.zip')
$ProgressPreference = 'SilentlyContinue'
try {
    Write-Host "Downloading ffmpeg + libav (fallback codec + decode sidecar) from $url"
    Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing

    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($actual -ne $sha) {
        throw "ffmpeg checksum mismatch`n  expected $sha`n  actual   $actual"
    }
    Write-Host "Checksum OK."

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($tmp)
    try {
        $wanted = $zip.Entries | Where-Object {
            ($_.FullName -match '/bin/[^/]+\.dll$') -or
            ($_.FullName -match '/bin/ffmpeg\.exe$') -or
            ($_.FullName -match '/bin/ffprobe\.exe$')
        }
        if (-not $wanted) { throw "archive contains no bin/ executables or DLLs — wrong build?" }
        $count = 0
        foreach ($entry in $wanted) {
            $out = Join-Path $Dest ([System.IO.Path]::GetFileName($entry.FullName))
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $out, $true)
            Write-Host "Installed $([System.IO.Path]::GetFileName($entry.FullName)) ($($entry.Length) bytes)"
            $count++
        }
        foreach ($req in $needExe) {
            if (-not (Test-Path (Join-Path $Dest $req))) { throw "missing $req after extract" }
        }
        if (-not (Get-ChildItem -Path $Dest -Filter 'avcodec*.dll' -ErrorAction SilentlyContinue)) {
            throw "missing avcodec*.dll after extract — sidecar would fail to load"
        }
        Write-Host "ffmpeg + libav ready in $Dest ($count files)"
    }
    finally {
        $zip.Dispose()
    }
}
finally {
    if (Test-Path $tmp) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue }
}
