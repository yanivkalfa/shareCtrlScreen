# run-request.ps1 - lane worker. Runs one request's steps in its worktree and
# writes <name>.result into done/. Started by drain.ps1; never by hand.
param([Parameter(Mandatory = $true)][string]$Request)

$ErrorActionPreference = 'Continue'
$Lane = Split-Path -Parent $MyInvocation.MyCommand.Path
$Running = Join-Path $Lane 'running'
$Done = Join-Path $Lane 'done'
$Logs = Join-Path $Lane 'logs'
$Name = [IO.Path]::GetFileNameWithoutExtension($Request)
$StallMinutes = 30
$HardCapMinutes = 180
$Jobs = 4

function Log($msg) {
    $line = "{0} [{1}] {2}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Name, $msg
    Add-Content -LiteralPath (Join-Path $Logs 'lane.log') -Value $line -Encoding utf8
}

function ToWsl($winPath) {
    $p = $winPath -replace '\\', '/'
    if ($p -match '^([A-Za-z]):/(.*)$') { return "/mnt/" + $Matches[1].ToLower() + "/" + $Matches[2] }
    return $p
}

function WriteLf($path, $text) {
    [IO.File]::WriteAllText($path, ($text -replace "`r`n", "`n"), (New-Object Text.UTF8Encoding($false)))
}

Set-Content -LiteralPath (Join-Path $Running "$Name.pid") -Value $PID -Encoding ascii
$started = Get-Date
Log "worker start pid=$PID"

# ---- parse -----------------------------------------------------------------
$meta = @{}; $cmds = @(); $inBody = $false
foreach ($l in (Get-Content -LiteralPath $Request -Encoding UTF8)) {
    $t = $l.Trim()
    if (-not $inBody) {
        if ($t -eq '---') { $inBody = $true; continue }
        if ($t -eq '' -or $t.StartsWith('#')) { continue }
        $i = $t.IndexOf(':'); if ($i -lt 0) { continue }
        $meta[$t.Substring(0, $i).Trim().ToLower()] = ($t.Substring($i + 1) -replace '#.*$', '').Trim()
    } elseif ($t -ne '' -and -not $t.StartsWith('#')) { $cmds += $t }
}

$task = $meta['task']; $side = $meta['side']; $cwd = $meta['cwd']
$errors = @()
if (-not $task) { $errors += 'missing task:' }
if ($side -ne 'windows' -and $side -ne 'wsl') { $errors += "side must be windows or wsl (got '$side')" }
if (-not $cwd -or -not (Test-Path -LiteralPath $cwd -PathType Container)) { $errors += "cwd missing or not a directory: '$cwd'" }
if ($cmds.Count -eq 0) { $errors += 'no commands after ---' }
foreach ($c in $cmds) {
    if ($c -notmatch '^(cargo|npm|npx)(\.exe)?(\s|$)') { $errors += "not a cargo/npm/npx command: $c" }
    if ($c -match '[;&|<>`]|\$\(') { $errors += "shell metacharacters refused: $c" }
}

$wslTarget = $meta['wsl_target']
if (-not $wslTarget) {
    if ($task -eq 'BASE') { $wslTarget = '$HOME/sct/base' } else { $wslTarget = "`$HOME/sct/$task" }
}

$commit = ''
if (-not $errors.Count) { $commit = (& git -C $cwd rev-parse --short HEAD 2>$null) -join '' }

# ---- run steps -------------------------------------------------------------
$stepLines = @(); $status = 'PASS'; $failTail = ''; $failedStep = 0
if ($errors.Count) {
    $status = 'FAIL'
    $stepLines += ($errors | ForEach-Object { "refused: $_" })
    Log "refused: $($errors -join ' | ')"
} else {
    for ($i = 1; $i -le $cmds.Count; $i++) {
        $cmd = $cmds[$i - 1]
        $log = Join-Path $Logs "$Name.step$i.log"
        if (Test-Path -LiteralPath $log) { Remove-Item -LiteralPath $log -Force }
        $pidFile = Join-Path $Running "$Name.step$i.pgid"
        $t0 = Get-Date
        Log "step $i start: $cmd"

        if ($side -eq 'windows') {
            $script = Join-Path $Running "$Name.step$i.cmd"
            $body = "@echo off`r`nset CARGO_BUILD_JOBS=$Jobs`r`ncd /d `"$cwd`"`r`n$cmd >> `"$log`" 2>&1`r`nexit /b %ERRORLEVEL%`r`n"
            [IO.File]::WriteAllText($script, $body, (New-Object Text.UTF8Encoding($false)))
            $p = Start-Process -FilePath 'cmd.exe' -ArgumentList @('/d', '/c', "`"$script`"") -PassThru -WindowStyle Hidden
        } else {
            $script = Join-Path $Running "$Name.step$i.sh"
            $body = @"
#!/bin/bash
source "`$HOME/.cargo/env"
export CARGO_BUILD_JOBS=$Jobs
export CARGO_TARGET_DIR="$wslTarget"
mkdir -p "`$CARGO_TARGET_DIR"
cd "$(ToWsl $cwd)" || exit 97
echo `$`$ > "$(ToWsl $pidFile)"
exec >> "$(ToWsl $log)" 2>&1
exec $cmd
"@
            WriteLf $script $body
            $p = Start-Process -FilePath 'wsl.exe' -ArgumentList @('-e', 'bash', '-c', "`"setsid -w bash '$(ToWsl $script)'`"") -PassThru -WindowStyle Hidden
        }
        $null = $p.Handle

        $lastSize = -1; $lastChange = Get-Date; $outcome = ''
        while (-not $p.HasExited) {
            Start-Sleep -Seconds 10
            $size = 0
            $item = Get-Item -LiteralPath $log -ErrorAction SilentlyContinue
            if ($item) { $size = $item.Length }
            if ($size -ne $lastSize) { $lastSize = $size; $lastChange = Get-Date }
            $now = Get-Date
            if (($now - $lastChange).TotalMinutes -ge $StallMinutes) { $outcome = 'STUCK' }
            elseif (($now - $t0).TotalMinutes -ge $HardCapMinutes) { $outcome = 'TIMEOUT' }
            if ($outcome) {
                Log "step $i $outcome - killing"
                if ($side -eq 'wsl' -and (Test-Path -LiteralPath $pidFile)) {
                    $pg = (Get-Content -LiteralPath $pidFile -Raw).Trim()
                    & wsl.exe -e bash -c "kill -TERM -- -$pg 2>/dev/null; sleep 3; kill -KILL -- -$pg 2>/dev/null" 2>$null
                }
                & taskkill.exe /T /F /PID $p.Id 2>$null | Out-Null
                Start-Sleep -Seconds 2
                break
            }
        }
        $dur = (Get-Date) - $t0
        $durStr = '{0:hh\:mm\:ss}' -f $dur
        if (-not $outcome) {
            $code = $p.ExitCode
            if ($code -eq 0) { $outcome = 'PASS' } else { $outcome = "FAIL (exit $code)" }
        } else { $outcome = "FAIL ($outcome)" }
        Remove-Item -LiteralPath $script, $pidFile -Force -ErrorAction SilentlyContinue
        $stepLines += "step {0}: {1}  {2}  -- {3}  [log: {4}]" -f $i, $outcome, $durStr, $cmd, $log
        Log "step $i $outcome in $durStr"
        if ($outcome -ne 'PASS') {
            $status = 'FAIL'; $failedStep = $i
            if (Test-Path -LiteralPath $log) { $failTail = (Get-Content -LiteralPath $log -Tail 150) -join "`n" }
            for ($j = $i + 1; $j -le $cmds.Count; $j++) { $stepLines += "step {0}: SKIPPED  -- {1}" -f $j, $cmds[$j - 1] }
            break
        }
    }
}

# ---- result ----------------------------------------------------------------
$finished = Get-Date
$total = '{0:hh\:mm\:ss}' -f ($finished - $started)
$out = @()
$out += "STATUS: $status"
$out += "request: $Name"
$out += "task: $task"
$out += "side: $side"
$out += "cwd: $cwd"
$out += "commit: $commit"
$out += "started: $($started.ToString('yyyy-MM-dd HH:mm:ss'))  finished: $($finished.ToString('yyyy-MM-dd HH:mm:ss'))  total: $total"
$out += ''
$out += $stepLines
if ($failTail) {
    $out += ''
    $out += "--- last 150 lines of step $failedStep (full log in the path above) ---"
    $out += $failTail
}
# The .result is written first and atomically (temp + rename) so a watcher on done/ sees a complete
# file; only then do the pid file and the request go, so the drain's reaper can never mistake a
# finished worker for a dead one (it also refuses to reap when a .result already exists).
$tmp = Join-Path $Logs "$Name.result.tmp"
[IO.File]::WriteAllText($tmp, (($out -join "`r`n") + "`r`n"), (New-Object Text.UTF8Encoding($false)))
Move-Item -LiteralPath $tmp -Destination (Join-Path $Done "$Name.result") -Force
Remove-Item -LiteralPath (Join-Path $Running "$Name.pid") -Force -ErrorAction SilentlyContinue
Move-Item -LiteralPath $Request -Destination (Join-Path $Done "$Name.request") -Force
Log "done: $status in $total"
