
[CmdletBinding()]
param(
    [string]$Ffmpeg = 'ffmpeg',
    [string]$Ffprobe = 'ffprobe'
)

$ErrorActionPreference = 'Continue'

$dir = Join-Path $PSScriptRoot '..\crates\vidcull-parser\tests\fixtures\allintra-mp4-e2e'
New-Item -ItemType Directory -Force -Path $dir | Out-Null

$clips = @(
    @{
        name   = 'clip_h264'
        size   = '160x90'
        codec  = 'libx264'
        params = @('-x264-params', 'keyint=1:min-keyint=1:scenecut=0:bframes=0:log-level=error')
        extra  = @('-profile:v', 'baseline')
    },
    @{
        name   = 'clip_h265'
        size   = '256x144'
        codec  = 'libx265'
        params = @('-x265-params', 'keyint=1:min-keyint=1:scenecut=0:open-gop=0:bframes=0:aq-mode=0:wpp=0:qp=27:log-level=error')
        extra  = @()
    }
)

function Test-HasStss {
    param([string]$Path)

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $len = $bytes.Length

    function Read-U32 {
        param([byte[]]$B, [int]$Off)
        return ([uint32]$B[$Off] -shl 24) -bor ([uint32]$B[$Off + 1] -shl 16) `
            -bor ([uint32]$B[$Off + 2] -shl 8) -bor ([uint32]$B[$Off + 3])
    }

    $containers = @('moov', 'trak', 'mdia', 'minf', 'stbl')
    function Find-Stss {
        param([byte[]]$B, [int]$Start, [int]$End)
        $pos = $Start
        while ($pos + 8 -le $End) {
            $size = Read-U32 $B $pos
            $type = [System.Text.Encoding]::ASCII.GetString($B, $pos + 4, 4)
            $headerLen = 8
            if ($size -eq 1) {
                $size = [int](Read-U32 $B ($pos + 12))
                $headerLen = 16
            }
            elseif ($size -eq 0) {
                $size = $End - $pos
            }
            if ($type -eq 'stss') { return $true }
            if ($containers -contains $type) {
                if (Find-Stss $B ($pos + $headerLen) ($pos + $size)) { return $true }
            }
            if ($size -le 0) { break }
            $pos += $size
        }
        return $false
    }

    return (Find-Stss $bytes 0 $len)
}

foreach ($c in $clips) {
    $mp4 = Join-Path $dir "$($c.name).mp4"
    $gray = Join-Path $dir "$($c.name).gray8"

    $args = @('-hide_banner', '-v', 'error', '-y', '-f', 'lavfi',
        '-i', "testsrc2=size=$($c.size):rate=24:duration=6",
        '-c:v', $c.codec, '-pix_fmt', 'yuv420p') + $c.extra + $c.params + @($mp4)
    & $Ffmpeg @args

    if (Test-HasStss $mp4) {
        throw "FIXTURE INVALID: $mp4 carries an stss box; all-intra encode should omit it. The no-stss seam is not exercised."
    }
    Write-Host "verified $($c.name).mp4 has NO stss box"

    $frames = & $Ffprobe -v error -select_streams v:0 -show_entries frame=key_frame `
        -of csv=p=0 $mp4
    $flags = @($frames | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
    $nonKey = @($flags | Where-Object { $_ -eq '0' })
    if ($nonKey.Count -gt 0) {
        throw "FIXTURE INVALID: $mp4 has $($nonKey.Count) non-keyframe(s); expected all-intra."
    }
    Write-Host "verified $($c.name).mp4 is all-intra ($($flags.Count) frames, all key)"

    if (Test-Path $gray) { Remove-Item $gray }
    foreach ($ss in @('0.000', '2.500', '5.000')) {
        $tmp = New-TemporaryFile
        & $Ffmpeg -y -v error -hide_banner -nostdin -ss $ss -i $mp4 `
            -frames:v 1 -an -vf format=gray -f rawvideo -pix_fmt gray $tmp.FullName
        Get-Content -Path $tmp.FullName -Encoding Byte -Raw |
            Add-Content -Path $gray -Encoding Byte
        Remove-Item $tmp.FullName
    }
    Write-Host "wrote $mp4 and $gray"
}
