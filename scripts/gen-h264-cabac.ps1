
[CmdletBinding()]
param(
    [string]$Ffmpeg = 'ffmpeg'
)

$ErrorActionPreference = 'Continue'
$repoRoot = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $repoRoot 'crates/vidcull-parser/tests/fixtures/h264-cabac'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$corpus = @(
    @{ name = 'main_testsrc_64x64'; size = '64x64';   filter = 'testsrc';   opts = ''; crf = 23; profile = 'main'; x264 = 'no-8x8dct=1' },
    @{ name = 'main_bars_176x144';  size = '176x144'; filter = 'smptebars'; opts = ''; crf = 23; profile = 'main'; x264 = 'no-8x8dct=1' },
    @{ name = 'main_crop_160x90';   size = '160x90';  filter = 'testsrc2';  opts = ''; crf = 27; profile = 'main'; x264 = 'no-8x8dct=1' },
    @{ name = 'high_testsrc_96x64';  size = '96x64';  filter = 'testsrc';   opts = ''; crf = 23; profile = 'high'; x264 = '8x8dct=1' },
    @{ name = 'high_bars_176x144';   size = '176x144'; filter = 'smptebars'; opts = ''; crf = 23; profile = 'high'; x264 = '8x8dct=1' }
)

foreach ($c in $corpus) {
    $h264 = Join-Path $outDir "$($c.name).h264"
    $y8 = Join-Path $outDir "$($c.name).y8"
    $gray8 = Join-Path $outDir "$($c.name).gray8"
    $lavfi = "$($c.filter)=size=$($c.size):rate=1$($c.opts)"

    & $Ffmpeg -y -f lavfi -i $lavfi -frames:v 1 `
        -c:v libx264 -coder 1 -profile:v $c.profile -g 1 -bf 0 -pix_fmt yuv420p -crf $c.crf `
        -x264-params $c.x264 `
        -f h264 $h264 2>$null
    if ($LASTEXITCODE -ne 0) { throw "encode failed for $($c.name)" }

    & $Ffmpeg -y -i $h264 -frames:v 1 -vf extractplanes=y -f rawvideo -pix_fmt gray $y8 2>$null
    if ($LASTEXITCODE -ne 0) { throw "reference extract failed for $($c.name)" }

    & $Ffmpeg -y -i $h264 -frames:v 1 -an -vf format=gray -f rawvideo -pix_fmt gray $gray8 2>$null
    if ($LASTEXITCODE -ne 0) { throw "gray reference extract failed for $($c.name)" }

    Write-Host "generated $($c.name) ($($c.size), $($c.profile))"
}

Write-Host "cabac conformance corpus written to $outDir"
