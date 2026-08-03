
[CmdletBinding()]
param(
    [Parameter(Mandatory = $false)]
    [string]$LibraryPath,

    [string]$DaemonExe = '',

    [int]$SampleIntervalSec = 15,

    [int]$TimeoutSec = 3600,

    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'


$THREAD_STACK_MB = 1.0   # Windows default stack per thread


$scriptDir  = Split-Path -Parent $MyInvocation.MyCommand.Path
$worktree   = Split-Path -Parent $scriptDir   # scripts/ -> repo root

$candidateExes = @(
    (Join-Path $worktree 'target-agent-ac0\debug\vidcull-daemon.exe'),
    (Join-Path $worktree 'target\debug\vidcull-daemon.exe')
)

if ($DaemonExe -eq '') {
    foreach ($c in $candidateExes) {
        if (Test-Path $c) { $DaemonExe = $c; break }
    }
    if ($DaemonExe -eq '') {
        $DaemonExe = $candidateExes[0]
    }
}


$argErrors = @()

if (-not $LibraryPath) {
    $argErrors += "Missing required argument: -LibraryPath <path to video library>"
} elseif (-not (Test-Path $LibraryPath)) {
    $argErrors += "LibraryPath does not exist: $LibraryPath"
}

if ($SampleIntervalSec -lt 1) {
    $argErrors += "-SampleIntervalSec must be >= 1 (got $SampleIntervalSec)"
}
if ($TimeoutSec -lt 10) {
    $argErrors += "-TimeoutSec must be >= 10 (got $TimeoutSec)"
}


if ($DryRun) {
    Write-Host "=== measure-index-rss DRY-RUN ==="
    Write-Host ""

    if ($argErrors.Count -gt 0) {
        Write-Host "Arg validation FAILED:"
        foreach ($e in $argErrors) { Write-Host "  ERROR: $e" }
        Write-Host ""
    } else {
        Write-Host "Arg validation OK"
        Write-Host ""
        Write-Host "Resolved paths:"
        Write-Host "  LibraryPath       : $LibraryPath"
        Write-Host "  DaemonExe         : $DaemonExe"
        Write-Host "  worktree-root     : $worktree"
        Write-Host "  SampleIntervalSec : $SampleIntervalSec"
        Write-Host "  TimeoutSec        : $TimeoutSec"
        Write-Host ""
        $daemonExists = Test-Path $DaemonExe
        Write-Host "daemon-exe $(if ($daemonExists) { 'EXISTS' } else { 'NOT FOUND (build first)' }) at $DaemonExe"
        if (-not $daemonExists) {
            Write-Host "  Build: cargo build -p vidcull-daemon"
        }
        Write-Host ""
        Write-Host "Daemon env that would be set:"
        $fakePid = "<PID>"
        Write-Host "  VIDCULL_IPC      = \\.\pipe\vidcull-rss-measure-$fakePid"
        Write-Host "  VIDCULL_DB       = `$env:TEMP\av-rss-$fakePid\measure.db"
        Write-Host "  VIDCULL_WATCH    = $LibraryPath"
        Write-Host "  VIDCULL_THUMB_DIR= `$env:TEMP\av-rss-$fakePid\thumbs"
        Write-Host "  VIDCULL_PARTIAL_CLIPS = 1"
        Write-Host ""
        Write-Host "Decomposition formula:"
        Write-Host "  thread_overhead_MB = peak_threads * $THREAD_STACK_MB"
        Write-Host "  per_file_buffer_MB = peak_RSS_MB - thread_overhead_MB - baseline_RSS_MB"
        Write-Host ""
        Write-Host "Stage-2 decision gate (fill in after measurement):"
        Write-Host "  If per_file_buffer_MB / (# files indexed) > ~2 MB/file -> Stage 2 needed"
    }
    exit 0
}

if ($argErrors.Count -gt 0) {
    foreach ($e in $argErrors) { Write-Error $e }
    exit 2
}

if (-not (Test-Path $DaemonExe)) {
    Write-Error "daemon-exe not found at $DaemonExe`nBuild with: cargo build -p vidcull-daemon"
    exit 2
}


function Log($msg) {
    $ts = (Get-Date).ToString("HH:mm:ss")
    Write-Host "[$ts] [measure-rss] $msg"
}

function MB($bytes) {
    return [math]::Round($bytes / 1MB, 1)
}


Log "Checking for orphaned vidcull-daemon processes..."
$orphans = Get-Process -Name 'vidcull-daemon' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and ($_.Path -match '\\target[^\\]*\\') }
if ($orphans) {
    Log "Found $($orphans.Count) orphan(s) — killing before measurement."
    foreach ($o in $orphans) {
        try { $o.Kill(); Log "  Killed PID $($o.Id)" }
        catch { Log "  Could not kill PID $($o.Id): $_" }
    }
    Start-Sleep -Seconds 1
} else {
    Log "No orphans found."
}


$runId   = $PID
$workDir = Join-Path $env:TEMP "av-rss-$runId"
New-Item -ItemType Directory -Path $workDir -Force | Out-Null
$dbPath    = Join-Path $workDir 'measure.db'
$thumbDir  = Join-Path $workDir 'thumbs'
$logFile   = Join-Path $workDir 'daemon.log'
New-Item -ItemType Directory -Path $thumbDir -Force | Out-Null

$ipcPipe = "\\.\pipe\vidcull-rss-measure-$runId"

Log "Work dir : $workDir"
Log "IPC pipe : $ipcPipe"
Log "Library  : $LibraryPath"
Log "Daemon   : $DaemonExe"


$savedEnv = @{}
$setEnv = @{
    'VIDCULL_IPC'          = $ipcPipe
    'VIDCULL_DB'           = $dbPath
    'VIDCULL_WATCH'        = $LibraryPath
    'VIDCULL_THUMB_DIR'    = $thumbDir
    'VIDCULL_PARTIAL_CLIPS'= '1'
}
foreach ($k in $setEnv.Keys) {
    $savedEnv[$k] = [System.Environment]::GetEnvironmentVariable($k)
    [System.Environment]::SetEnvironmentVariable($k, $setEnv[$k])
}

Log "Spawning daemon (stdout+stderr -> $logFile) ..."
$daemon = Start-Process `
    -FilePath   $DaemonExe `
    -RedirectStandardOutput $logFile `
    -RedirectStandardError  "$logFile.err" `
    -PassThru `
    -WindowStyle Hidden

foreach ($k in $savedEnv.Keys) {
    [System.Environment]::SetEnvironmentVariable($k, $savedEnv[$k])
}

Start-Sleep -Seconds 2
if ($daemon.HasExited) {
    $ec = $daemon.ExitCode
    Log "ERROR: daemon exited immediately (code=$ec). Log:"
    Get-Content $logFile -ErrorAction SilentlyContinue | ForEach-Object { Log "  $_" }
    Get-Content "$logFile.err" -ErrorAction SilentlyContinue | ForEach-Object { Log "  STDERR: $_" }
    exit 1
}
Log "Daemon PID $($daemon.Id) started."


$samples       = [System.Collections.Generic.List[PSCustomObject]]::new()
$deadline      = (Get-Date).AddSeconds($TimeoutSec)
$peakRssBytes  = 0L
$peakThreads   = 0
$baselineRss   = 0L
$baselineSet   = $false
$decodedCount  = 0
$prevDecoded   = -1

Log "Sampling every ${SampleIntervalSec}s for up to ${TimeoutSec}s ..."
Log "$('{0,-8} {1,10} {2,8} {3,8} {4,12}' -f 'elapsed', 'decoded', 'RSS_MB', 'threads', 'peak_RSS_MB')"

$startTime = Get-Date

while ($true) {
    $daemon.Refresh()
    if ($daemon.HasExited) {
        Log "Daemon exited (code=$($daemon.ExitCode)) — sampling complete."
        break
    }

    $rssBytes = $daemon.WorkingSet64
    $threads  = $daemon.Threads.Count

    if (-not $baselineSet) {
        $baselineRss = $rssBytes
        $baselineSet = $true
        Log "Baseline RSS: $(MB $baselineRss) MB"
    }

    if ($rssBytes -gt $peakRssBytes) { $peakRssBytes = $rssBytes }
    if ($threads  -gt $peakThreads)  { $peakThreads  = $threads  }

    if (Test-Path $logFile) {
        $decodedCount = (Select-String -Path $logFile -Pattern 'file decoded and fingerprinted' -ErrorAction SilentlyContinue).Count
        if ($null -eq $decodedCount) { $decodedCount = 0 }
    }

    $elapsed = [int]((Get-Date) - $startTime).TotalSeconds

    $sample = [PSCustomObject]@{
        ElapsedSec = $elapsed
        DecodedFiles = $decodedCount
        RssBytes   = $rssBytes
        Threads    = $threads
    }
    $samples.Add($sample)

    if ($decodedCount -ne $prevDecoded -or $samples.Count % 4 -eq 1) {
        $row = '{0,-8} {1,10} {2,8} {3,8} {4,12}' -f `
            "${elapsed}s", $decodedCount, (MB $rssBytes), $threads, (MB $peakRssBytes)
        Log $row
        $prevDecoded = $decodedCount
    }

    if ((Get-Date) -gt $deadline) {
        Log "TIMEOUT after ${TimeoutSec}s — stopping."
        break
    }

    Start-Sleep -Seconds $SampleIntervalSec
}

$daemon.Refresh()
if (-not $daemon.HasExited) {
    $rssBytes = $daemon.WorkingSet64
    $threads  = $daemon.Threads.Count
    if ($rssBytes -gt $peakRssBytes) { $peakRssBytes = $rssBytes }
    if ($threads  -gt $peakThreads)  { $peakThreads  = $threads  }
}

if (Test-Path $logFile) {
    $finalDecoded = (Select-String -Path $logFile -Pattern 'file decoded and fingerprinted' -ErrorAction SilentlyContinue).Count
    if ($null -eq $finalDecoded) { $finalDecoded = 0 }
} else {
    $finalDecoded = 0
}


$threadOverheadMB = [math]::Round($peakThreads * $THREAD_STACK_MB, 1)
$baselineMB       = MB $baselineRss
$peakMB           = MB $peakRssBytes

$perFileMB = [math]::Round($peakMB - $threadOverheadMB - $baselineMB, 1)
if ($perFileMB -lt 0) { $perFileMB = 0.0 }

$perFilePerFileMB = if ($finalDecoded -gt 0) { [math]::Round($perFileMB / $finalDecoded, 2) } else { 'N/A (0 files decoded)' }

$totalSec = [int]((Get-Date) - $startTime).TotalSeconds

Write-Host ""
Write-Host "=========================================================="
Write-Host " measure-index-rss RESULTS"
Write-Host "=========================================================="
Write-Host " Library          : $LibraryPath"
Write-Host " Run duration     : ${totalSec}s"
Write-Host " Files decoded    : $finalDecoded"
Write-Host "----------------------------------------------------------"
Write-Host " Baseline RSS     : $baselineMB MB   (after 2s warmup)"
Write-Host " Peak RSS         : $peakMB MB"
Write-Host " Peak threads     : $peakThreads"
Write-Host "----------------------------------------------------------"
Write-Host " DECOMPOSITION (formula: peak - thread_overhead - baseline)"
Write-Host "   thread_overhead = $peakThreads threads * ${THREAD_STACK_MB} MB/thread = $threadOverheadMB MB"
Write-Host "   per_file_buffer = $peakMB - $threadOverheadMB - $baselineMB = $perFileMB MB"
if ($finalDecoded -gt 0) {
    Write-Host "   per_file/file   = $perFileMB MB / $finalDecoded files = $perFilePerFileMB MB/file"
}
Write-Host "----------------------------------------------------------"
Write-Host " STAGE-2 GATE"
if ($finalDecoded -gt 0 -and $perFilePerFileMB -is [double]) {
    if ($perFilePerFileMB -gt 2.0) {
        Write-Host "   per_file/file ($perFilePerFileMB MB) > 2.0 MB  -> Stage 2 (streaming) INDICATED"
    } else {
        Write-Host "   per_file/file ($perFilePerFileMB MB) <= 2.0 MB -> Stage 2 NOT needed"
    }
} else {
    Write-Host "   Cannot evaluate: $finalDecoded file(s) decoded"
}
Write-Host "=========================================================="
Write-Host ""

$csvPath = Join-Path $workDir 'samples.csv'
$samples | Export-Csv -Path $csvPath -NoTypeInformation
Write-Host "Sample data written to: $csvPath"
Write-Host "Daemon log: $logFile"
Write-Host ""


Log "Cleaning up..."

$daemon.Refresh()
if (-not $daemon.HasExited) {
    try {
        $daemon.Kill()
        Log "Daemon PID $($daemon.Id) killed."
    } catch {
        Log "Could not kill daemon: $_"
    }
}

try { Remove-Item -Recurse -Force $workDir -ErrorAction SilentlyContinue } catch {}
Log "Temp dir removed: $workDir"
Log "Done."
