
[CmdletBinding()]
param(
    [string]$Ffmpeg = 'ffmpeg'
)

$ErrorActionPreference = 'Continue'
$repoRoot = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $repoRoot 'crates/vidcull-parser/tests/fixtures/h264-native-e2e'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$baselineArgs = @('-coder', '0', '-profile:v', 'baseline', '-bf', '0')
$highArgs = @('-coder', '1', '-profile:v', 'high', '-bf', '0')
$corpus = @(
    @{ name = 'testsrc2_160_90';      size = '160x90';  filter = 'testsrc2';  codec = $baselineArgs },
    @{ name = 'smptebars_176_144';    size = '176x144'; filter = 'smptebars'; codec = $baselineArgs },
    @{ name = 'testsrc2_high_160_90'; size = '160x90';  filter = 'testsrc2';  codec = $highArgs }
)

$gridSeconds = @('0.000', '2.500', '5.000')

foreach ($c in $corpus) {
    $mp4 = Join-Path $outDir "$($c.name).mp4"
    $gray8 = Join-Path $outDir "$($c.name).gray8"
    $lavfi = "$($c.filter)=size=$($c.size):rate=10"

    & $Ffmpeg -y -f lavfi -i $lavfi -t 6 `
        -c:v libx264 @($c.codec) -g 25 -keyint_min 25 `
        -pix_fmt yuv420p -crf 23 $mp4 2>$null
    if ($LASTEXITCODE -ne 0) { throw "encode failed for $($c.name)" }

    $mkv = Join-Path $outDir "$($c.name).mkv"
    & $Ffmpeg -y -i $mp4 -map 0:v:0 -c copy $mkv 2>$null
    if ($LASTEXITCODE -ne 0) { throw "remux to mkv failed for $($c.name)" }

    if (Test-Path $gray8) { Remove-Item $gray8 -Force }
    foreach ($s in $gridSeconds) {
        $tmp = Join-Path $outDir "_grid_$s.bin"
        & $Ffmpeg -v error -hide_banner -nostdin -ss $s -i $mp4 `
            -frames:v 1 -an -vf format=gray -f rawvideo -pix_fmt gray -y $tmp 2>$null
        if ($LASTEXITCODE -ne 0) { throw "reference decode at ${s}s failed for $($c.name)" }
        Add-Content -Path $gray8 -Value ([System.IO.File]::ReadAllBytes($tmp)) -Encoding Byte
        Remove-Item $tmp -Force
    }

    Write-Host "generated $($c.name) ($($c.size), $($gridSeconds.Count) plan frames, mp4+mkv)"
}

Write-Host "native-e2e corpus written to $outDir"
