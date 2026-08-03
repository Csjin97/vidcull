
[CmdletBinding()]
param(
    [string]$Ffmpeg = (Join-Path $PSScriptRoot '..\vendor\ffmpeg\windows-x86_64\ffmpeg.exe')
)

$ErrorActionPreference = 'Continue'

$dir = Join-Path $PSScriptRoot '..\crates\vidcull-parser\tests\fixtures\hevc-wpp-e2e'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

$clips = @(
    @{ name = 'wpp';  size = '256x192'; qp = 27 },
    @{ name = 'wpp2'; size = '320x256'; qp = 32 }
)

foreach ($c in $clips) {
    $mkv = Join-Path $dir "$($c.name).mkv"
    $gray = Join-Path $dir "$($c.name).gray8"

    $x265 = "ctu=64:keyint=60:min-keyint=60:scenecut=0:open-gop=0:bframes=0:aq-mode=0:wpp=1:qp=$($c.qp):log-level=error"
    & $Ffmpeg -hide_banner -v error -y -f lavfi -i "testsrc2=size=$($c.size):rate=24:duration=6" `
        -c:v libx265 -pix_fmt yuv420p -profile:v main -x265-params $x265 $mkv

    if (Test-Path $gray) { Remove-Item $gray }
    foreach ($ss in @('0.000', '2.500', '5.000')) {
        $tmp = New-TemporaryFile
        & $Ffmpeg -y -v error -hide_banner -nostdin -ss $ss -i $mkv `
            -frames:v 1 -an -vf format=gray -f rawvideo -pix_fmt gray $tmp.FullName
        Get-Content -Path $tmp.FullName -Encoding Byte -Raw |
            Add-Content -Path $gray -Encoding Byte
        Remove-Item $tmp.FullName
    }
    Write-Host "wrote $mkv and $gray"
}
