
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$appRoot  = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $appRoot

$candidates = @(
    (Join-Path $appRoot  'src-tauri\target\release\vidcull-ui.exe'),
    (Join-Path $repoRoot 'target\release\vidcull-daemon.exe')
)

$binDir = Join-Path $appRoot 'src-tauri\binaries'
if (Test-Path $binDir) {
    Get-ChildItem -Path $binDir -Filter '*.exe' |
        ForEach-Object { $candidates += $_.FullName }
}

$username = $env:USERNAME

$patterns = @(
    [pscustomobject]@{ Label = 'C:\Users\ path';          Rx = [regex]'(?i)C:\\Users\\' }
    [pscustomobject]@{ Label = 'USERNAME in path';        Rx = [regex]('(?i)[\\/]' + [regex]::Escape($username) + '[\\/]') }
    [pscustomobject]@{ Label = '.cargo\registry path';    Rx = [regex]'(?i)\.cargo\\registry' }
    [pscustomobject]@{ Label = '.rustup\toolchains path'; Rx = [regex]'(?i)\.rustup\\toolchains' }
)

$latin1 = [System.Text.Encoding]::GetEncoding(28591)

$anyLeak = $false

foreach ($exePath in $candidates) {
    if (-not (Test-Path $exePath)) {
        Write-Host "SKIP  (not found): $exePath"
        continue
    }

    Write-Host "CHECK $exePath"

    $bytes = [System.IO.File]::ReadAllBytes($exePath)
    $text  = $latin1.GetString($bytes)

    foreach ($entry in $patterns) {
        $label   = $entry.Label
        $pattern = $entry.Rx

        $found = $pattern.Matches($text)
        if ($found.Count -eq 0) { continue }

        Write-Host ""
        Write-Host "LEAK  [$label] — $($found.Count) match(es) in $(Split-Path -Leaf $exePath)"
        $shown = 0
        foreach ($m in $found) {
            if ($shown -ge 5) { break }
            $start  = [Math]::Max(0, $m.Index - 40)
            $length = [Math]::Min($text.Length - $start, $m.Length + 80)
            $snippet = $text.Substring($start, $length) -replace '[^\x20-\x7E]', '.'
            Write-Host "  ...${snippet}..."
            $shown++
        }
        if ($found.Count -gt 5) {
            Write-Host "  (and $($found.Count - 5) more)"
        }

        $anyLeak = $true
    }
}

Write-Host ""
if ($anyLeak) {
    Write-Host "PE privacy: FAIL — personal/build-machine paths detected."
    Write-Host "Re-run build-release.ps1 to apply RUSTFLAGS remap-path-prefix."
    exit 1
} else {
    Write-Host "PE privacy: clean"
    exit 0
}
