
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$target   = Join-Path $repoRoot 'target-agent7'
$daemonExe = Join-Path $target 'debug\vidcull-daemon.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$valueName = 'vidcull'

$pidSuffix = $PID
$pipeMain  = "\\.\pipe\vidcull-autostart-$pidSuffix"
$dbPath    = Join-Path $env:TEMP "av-autostart-$pidSuffix.db"

$failures = New-Object System.Collections.Generic.List[string]
function Check($name, [bool]$ok, $detail = '') {
  if ($ok) { Write-Host "  PASS  $name" }
  else { $failures.Add("$name -- $detail"); Write-Host "  FAIL  $name -- $detail" }
}

function Get-RunValue {
  try { (Get-ItemProperty -Path $runKey -Name $valueName -ErrorAction Stop).$valueName }
  catch { $null }
}

if (-not (Test-Path $daemonExe)) {
  throw "daemon not built at $daemonExe"
}

$preExisting = Get-RunValue
$startState = if ($null -eq $preExisting) { '<absent>' } else { $preExisting }
Write-Host "[autostart] starting Run\$valueName state: $startState"

$spawned = New-Object System.Collections.Generic.List[System.Diagnostics.Process]
function Stop-Spawned {
  foreach ($p in $spawned) {
    if ($p -and -not $p.HasExited) {
      try { & taskkill /PID $p.Id /T /F 2>$null | Out-Null } catch {}
    }
  }
}

$manifest = Join-Path $repoRoot 'crates\vidcull-daemon\Cargo.toml'

try {
  $env:VIDCULL_IPC = $pipeMain
  $env:VIDCULL_DB  = $dbPath
  $env:CARGO_TARGET_DIR = $target
  Write-Host "[autostart] starting daemon (pipe=$pipeMain)"
  $daemon = Start-Process -FilePath $daemonExe -PassThru -WindowStyle Hidden
  $spawned.Add($daemon)
  Start-Sleep -Seconds 2
  if ($daemon.HasExited) { throw "daemon exited early (code=$($daemon.ExitCode))" }

  Write-Host "[autostart] toggling run_on_boot ON via SetSettings"
  $on = & cargo run --quiet --example verify_autostart --manifest-path $manifest -- on
  Write-Host "[autostart] daemon reply: $on"
  Start-Sleep -Milliseconds 500
  $afterOn = Get-RunValue
  Check 'toggling run_on_boot ON creates the HKCU Run value' ($null -ne $afterOn) "Run\$valueName still absent after ON"
  Write-Host "[autostart] Run\$valueName = $afterOn"

  $proxyOk = $false
  $proxyDetail = ''
  if ($null -ne $afterOn) {
    $proxyPipe = "\\.\pipe\vidcull-autostart-proxy-$pidSuffix"
    $proxyDb   = Join-Path $env:TEMP "av-autostart-proxy-$pidSuffix.db"
    $exePath = $afterOn.Trim('"')
    Write-Host "[autostart] reboot proxy: launching recorded command -> $exePath"
    $env:VIDCULL_IPC = $proxyPipe
    $env:VIDCULL_DB  = $proxyDb
    $proxy = Start-Process -FilePath $exePath -PassThru -WindowStyle Hidden
    $spawned.Add($proxy)
    Start-Sleep -Seconds 2
    if ($proxy.HasExited) {
      $proxyDetail = "proxy daemon exited early (code=$($proxy.ExitCode))"
    } else {
      try {
        $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $proxyPipe.Substring(9), [System.IO.Pipes.PipeDirection]::InOut)
        $client.Connect(5000)
        $proxyOk = $client.IsConnected
        $client.Dispose()
      } catch { $proxyDetail = "could not connect to proxy daemon pipe: $_" }
    }
    $env:VIDCULL_IPC = $pipeMain
    $env:VIDCULL_DB  = $dbPath
  }
  Check 'the recorded Run command starts a working daemon (reboot proxy)' $proxyOk $proxyDetail

  Write-Host "[autostart] toggling run_on_boot OFF via SetSettings"
  $off = & cargo run --quiet --example verify_autostart --manifest-path $manifest -- off
  Write-Host "[autostart] daemon reply: $off"
  Start-Sleep -Milliseconds 500
  $afterOff = Get-RunValue
  Check 'toggling run_on_boot OFF removes the HKCU Run value' ($null -eq $afterOff) "Run\$valueName still present after OFF: $afterOff"
}
finally {
  Stop-Spawned
  $now = Get-RunValue
  if ($null -eq $preExisting) {
    if ($null -ne $now) {
      Remove-ItemProperty -Path $runKey -Name $valueName -ErrorAction SilentlyContinue
      Write-Host "[autostart] cleanup: removed test Run\$valueName"
    }
  } else {
    Set-ItemProperty -Path $runKey -Name $valueName -Value $preExisting
    Write-Host "[autostart] cleanup: restored pre-existing Run\$valueName"
  }
  Remove-Item -Path $dbPath -ErrorAction SilentlyContinue
  Remove-Item -Path (Join-Path $env:TEMP "av-autostart-proxy-$pidSuffix.db") -ErrorAction SilentlyContinue
}

Write-Host ''
if ($failures.Count -gt 0) {
  Write-Host "$($failures.Count) check(s) failed:"
  foreach ($f in $failures) { Write-Host "  - $f" }
  exit 1
}
Write-Host 'All live-autostart checks passed.'
exit 0
