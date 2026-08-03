
[CmdletBinding()]
param(
    [string]$DbPath,

    [switch]$Names,

    [switch]$All,

    [string]$CargoTargetDir
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot 'Cargo.toml'

if ($env:LOCALAPPDATA) {
    $defaultDataDir = Join-Path $env:LOCALAPPDATA 'vidcull'
} else {
    $defaultDataDir = Join-Path $env:TEMP 'vidcull'
}
$defaultDbPath = Join-Path $defaultDataDir 'vidcull.db'

if (-not $DbPath) {
    if ($env:VIDCULL_DB) {
        $DbPath = $env:VIDCULL_DB
        Write-Host "No -DbPath given; using VIDCULL_DB env override: $DbPath"
    } else {
        $DbPath = $defaultDbPath
        Write-Host "No -DbPath given; using the daemon's default location:"
        Write-Host "  $DbPath"
    }
}

Write-Host ''
Write-Host 'Phase A whole-file shadow scan -- SAFE offline measurement'
Write-Host '===================================================================='
Write-Host 'This script will, in order:'
Write-Host "  1. Look for your real database at: $DbPath"
Write-Host '  2. COPY it (never touching the original) to a fresh temp directory'
Write-Host '  3. Build whole_shadow_scan with an isolated Cargo target dir'
Write-Host '  4. Run it READ-ONLY against the COPY only'
Write-Host '  5. Print the [whole-shadow] report below'
Write-Host ''

if (-not (Test-Path -LiteralPath $DbPath -PathType Leaf)) {
    Write-Host "ERROR: database file not found at:"
    Write-Host "  $DbPath"
    Write-Host ''
    Write-Host 'The vidcull daemon default database location is:'
    Write-Host "  $defaultDbPath"
    Write-Host '(or wherever $env:VIDCULL_DB points, if you run the daemon with that set).'
    Write-Host 'Pass -DbPath <path> to point at a different file.'
    exit 1
}

$workDir = Join-Path $env:TEMP ('vidcull-whole-shadow-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $workDir -Force | Out-Null
$dbCopy = Join-Path $workDir 'vidcull-copy.db'

Write-Host 'Copying database (source is only ever read):'
Write-Host "  from: $DbPath"
Write-Host "  to:   $dbCopy"
Copy-Item -LiteralPath $DbPath -Destination $dbCopy -Force

if (Test-Path -LiteralPath "$DbPath-wal") {
    Write-Host ''
    Write-Host "Note: '$DbPath-wal' exists next to the source (daemon likely running)."
    Write-Host '      The copy may be missing its most recent, not-yet-checkpointed writes.'
}

if (-not $CargoTargetDir) {
    $CargoTargetDir = Join-Path $env:LOCALAPPDATA 'vidcull-tools\whole-shadow-target'
}
Write-Host ''
Write-Host 'Building whole_shadow_scan (isolated CARGO_TARGET_DIR):'
Write-Host "  $CargoTargetDir"

$prevTargetDir = $env:CARGO_TARGET_DIR
$env:CARGO_TARGET_DIR = $CargoTargetDir
$scanExit = 1
try {
    $cargoArgs = @(
        'build', '--release',
        '--manifest-path', $manifestPath,
        '-p', 'vidcull-daemon',
        '--bin', 'whole_shadow_scan'
    )
    Write-Host "  cargo $($cargoArgs -join ' ')"
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed (exit code $LASTEXITCODE)"
    }

    $exe = Join-Path $CargoTargetDir 'release\whole_shadow_scan.exe'
    if (-not (Test-Path -LiteralPath $exe)) {
        throw "built binary not found at expected path: $exe"
    }

    $binArgs = @($dbCopy)
    if ($Names) { $binArgs += '--names' }
    if ($All) { $binArgs += '--all' }

    Write-Host ''
    Write-Host 'Running (read-only, against the COPY -- the original is untouched):'
    Write-Host "  $exe $($binArgs -join ' ')"
    Write-Host ''
    & $exe @binArgs
    $scanExit = $LASTEXITCODE
}
finally {
    $env:CARGO_TARGET_DIR = $prevTargetDir
}

Write-Host ''
Write-Host 'Temp working dir (DB copy + nothing else) left at:'
Write-Host "  $workDir"
Write-Host 'Delete it any time -- it is a copy, not your real database.'

exit $scanExit
