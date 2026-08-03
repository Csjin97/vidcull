
[CmdletBinding()]
param(
    [string]$Ffmpeg = 'ffmpeg'
)

$ErrorActionPreference = 'Continue'
$repoRoot = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $repoRoot 'crates/vidcull-parser/tests/fixtures/h264-conformance'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$corpus = @(
    @{ name = 'grad_48x32';    size = '48x32';   filter = 'gradients'; opts = ':x0=0:y0=0:x1=48:y1=32'; crf = 28 },
    @{ name = 'testsrc_64x64'; size = '64x64';   filter = 'testsrc';   opts = '';                       crf = 23 },
    @{ name = 'crop_160x90';   size = '160x90';  filter = 'testsrc2';  opts = '';                       crf = 23 },
    @{ name = 'bars_176x144';  size = '176x144'; filter = 'smptebars'; opts = '';                       crf = 23 }
)

foreach ($c in $corpus) {
    $h264 = Join-Path $outDir "$($c.name).h264"
    $y8 = Join-Path $outDir "$($c.name).y8"
    $gray8 = Join-Path $outDir "$($c.name).gray8"
    $lavfi = "$($c.filter)=size=$($c.size):rate=1$($c.opts)"

    & $Ffmpeg -y -f lavfi -i $lavfi -frames:v 1 `
        -c:v libx264 -coder 0 -profile:v baseline -g 1 -bf 0 -pix_fmt yuv420p -crf $c.crf `
        -f h264 $h264 2>$null
    if ($LASTEXITCODE -ne 0) { throw "encode failed for $($c.name)" }

    & $Ffmpeg -y -i $h264 -frames:v 1 -vf extractplanes=y -f rawvideo -pix_fmt gray $y8 2>$null
    if ($LASTEXITCODE -ne 0) { throw "reference extract failed for $($c.name)" }

    & $Ffmpeg -y -i $h264 -frames:v 1 -an -vf format=gray -f rawvideo -pix_fmt gray $gray8 2>$null
    if ($LASTEXITCODE -ne 0) { throw "gray reference extract failed for $($c.name)" }

    Write-Host "generated $($c.name) ($($c.size))"
}

$high = 'high8x8_160x90'
$highMp4 = Join-Path $outDir "$high.mp4"
$highH264 = Join-Path $outDir "$high.h264"
$highY8 = Join-Path $outDir "$high.y8"
$highGray8 = Join-Path $outDir "$high.gray8"

& $Ffmpeg -y -f lavfi -i 'testsrc2=size=160x90:rate=24:duration=6' `
    -c:v libx264 -pix_fmt yuv420p -profile:v high `
    -x264-params 'keyint=1:min-keyint=1:scenecut=0:bframes=0:log-level=error' `
    $highMp4 2>$null
if ($LASTEXITCODE -ne 0) { throw "high8x8 MP4 encode failed" }

& $Ffmpeg -y -ss 2.500 -i $highMp4 -frames:v 1 -c:v copy -bsf:v h264_mp4toannexb `
    -f h264 $highH264 2>$null
if ($LASTEXITCODE -ne 0) { throw "high8x8 Annex B demux failed" }
Remove-Item $highMp4 -ErrorAction SilentlyContinue

& $Ffmpeg -y -i $highH264 -frames:v 1 -vf extractplanes=y -f rawvideo -pix_fmt gray $highY8 2>$null
if ($LASTEXITCODE -ne 0) { throw "high8x8 reference extract failed" }
& $Ffmpeg -y -i $highH264 -frames:v 1 -an -vf format=gray -f rawvideo -pix_fmt gray $highGray8 2>$null
if ($LASTEXITCODE -ne 0) { throw "high8x8 gray reference extract failed" }
Write-Host "generated $high (160x90, High/CABAC 8x8)"

Write-Host "conformance corpus written to $outDir"
