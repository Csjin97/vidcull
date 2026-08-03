
[CmdletBinding()]
param(
    [string]$Platform = $(if ($IsLinux) { 'linux-x86_64' } else { 'windows-x86_64' }),
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot 'vendor/ffmpeg/MANIFEST.toml'
if (-not (Test-Path $manifestPath)) { throw "manifest not found: $manifestPath" }
$manifest = Get-Content $manifestPath -Raw

$escaped = [regex]::Escape($Platform)
$blockMatch = [regex]::Match(
    $manifest,
    "(?ms)^\[ffmpeg\.platforms\.$escaped\]\s*(.*?)(?=^\[|\z)"
)
if (-not $blockMatch.Success) { throw "platform '$Platform' not pinned in $manifestPath" }
$block = $blockMatch.Groups[1].Value

function Get-Field([string]$name, [string]$text) {
    $m = [regex]::Match($text, "(?m)^\s*$name\s*=\s*`"([^`"]+)`"")
    if (-not $m.Success) { throw "field '$name' missing for platform '$Platform'" }
    return $m.Groups[1].Value
}

$url = Get-Field 'url' $block

$isZip   = $url -match '\.zip$'
$isTarXz = $url -match '\.tar\.xz$'
if (-not $isZip -and -not $isTarXz) {
    throw "Unrecognised archive format for URL: $url (expected .zip or .tar.xz)"
}
$checksumField = if ($isZip) { 'zip_sha256' } else { 'tarxz_sha256' }
$sha256 = (Get-Field $checksumField $block).ToUpperInvariant()

$extractMatch = [regex]::Match($block, "(?ms)extract\s*=\s*\[(.*?)\]")
if (-not $extractMatch.Success) { throw "field 'extract' missing for platform '$Platform'" }
$extract = [regex]::Matches($extractMatch.Groups[1].Value, "`"([^`"]+)`"") | ForEach-Object { $_.Groups[1].Value }

$destDir = Join-Path $repoRoot "vendor/ffmpeg/$Platform"
New-Item -ItemType Directory -Force -Path $destDir | Out-Null

$present = $extract | ForEach-Object { Test-Path (Join-Path $destDir $_) }
if (($present -notcontains $false) -and -not $Force) {
    Write-Host "ffmpeg bundle already present in $destDir (use -Force to refresh)."
    return
}

$ext = if ($isZip) { '_download.zip' } else { '_download.tar.xz' }
$tmp = Join-Path $destDir $ext
Write-Host "Downloading $url"
$ProgressPreference = 'SilentlyContinue'
Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing

$actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToUpperInvariant()
if ($actual -ne $sha256) {
    Remove-Item $tmp -Force
    throw "SHA-256 mismatch for $Platform`n  expected $sha256`n  actual   $actual`nUpstream bytes changed; refusing to ship an unpinned ffmpeg."
}
Write-Host "Checksum OK ($sha256)"

if ($isZip) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($tmp)
    try {
        foreach ($name in $extract) {
            $entry = $archive.Entries | Where-Object { $_.FullName -match "/bin/$([regex]::Escape($name))$" } | Select-Object -First 1
            if (-not $entry) { throw "archive does not contain bin/$name" }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $destDir $name), $true)
            Write-Host "Extracted $name ($($entry.Length) bytes)"
        }
    } finally {
        $archive.Dispose()
        Remove-Item $tmp -Force
    }
} else {
    $destDirUnix = $destDir -replace '\\', '/'

    if ($IsLinux) {
        $tarArgs = @('-xf', $tmp, '-C', $destDir, '--wildcards')
        foreach ($name in $extract) { $tarArgs += "*/bin/$name" }
        & tar @tarArgs
        foreach ($name in $extract) {
            $nested = Get-ChildItem $destDir -Recurse -Filter $name | Select-Object -First 1
            if ($nested -and $nested.FullName -ne (Join-Path $destDir $name)) {
                Move-Item $nested.FullName (Join-Path $destDir $name) -Force
            }
            & chmod +x (Join-Path $destDir $name) 2>$null
            Write-Host "Extracted $name ($((Get-Item (Join-Path $destDir $name)).Length) bytes)"
        }
        Get-ChildItem $destDir -Directory | Remove-Item -Recurse -Force
    } else {
        $tmpWsl     = (& wsl wslpath -u $tmp.Replace('\', '/')) 2>$null
        if (-not $tmpWsl) { $tmpWsl = '/mnt/' + ($tmp -replace '\\','/' -replace '^([A-Za-z]):','$1').ToLower() }
        $tmpWsl = $tmpWsl.Trim()
        $destWsl = (& wsl wslpath -u $destDirUnix) 2>$null
        if (-not $destWsl) { $destWsl = '/mnt/' + ($destDirUnix -replace '^([A-Za-z]):','$1').ToLower() }
        $destWsl = $destWsl.Trim()

        $extractPatterns = ($extract | ForEach-Object { "'*/bin/$_'" }) -join ' '
        $chmodTargets = ($extract | ForEach-Object { "'$destWsl/$_'" }) -join ' '
        $script = "set -e; mkdir -p '$destWsl'; tar -xf '$tmpWsl' -C '$destWsl' --wildcards --strip-components=2 $extractPatterns; chmod +x $chmodTargets"
        wsl -- bash -c $script
        foreach ($name in $extract) {
            Write-Host "Extracted $name ($((Get-Item (Join-Path $destDir $name)).Length) bytes)"
        }
    }
    Remove-Item $tmp -Force
}
Write-Host "ffmpeg bundle ready in $destDir"
