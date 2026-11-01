# drain.ps1 - lane scheduler. Claims requests from queue/ (gate priority first,
# then oldest) and keeps at most N workers running (N = `slots` file, default 5).
# Reaps workers that died without writing a result. Started by lane-up.ps1.
$ErrorActionPreference = 'Continue'
$Lane = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Split-Path -Parent (Split-Path -Parent $Lane)   # the repository checkout
$Queue = Join-Path $Lane 'queue'
$Running = Join-Path $Lane 'running'
$Done = Join-Path $Lane 'done'
$Logs = Join-Path $Lane 'logs'
foreach ($d in @($Queue, $Running, $Done, $Logs)) { if (-not (Test-Path -LiteralPath $d)) { New-Item -ItemType Directory -Path $d | Out-Null } }
$Worker = Join-Path $Lane 'run-request.ps1'
$DefaultSlots = 5

function Log($msg) {
    $line = "{0} [drain] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $msg
    Add-Content -LiteralPath (Join-Path $Logs 'lane.log') -Value $line -Encoding utf8
}

function Slots {
    $f = Join-Path $Lane 'slots'
    if (Test-Path -LiteralPath $f) {
        $n = 0
        if ([int]::TryParse((Get-Content -LiteralPath $f -Raw).Trim(), [ref]$n) -and $n -ge 1) { return $n }
    }
    return $DefaultSlots
}

function Priority($file) {
    foreach ($l in (Get-Content -LiteralPath $file.FullName -TotalCount 40 -ErrorAction SilentlyContinue)) {
        $t = $l.Trim()
        if ($t -eq '---') { break }
        if ($t -match '^priority\s*:\s*gate') { return 0 }
    }
    return 1
}

Set-Content -LiteralPath (Join-Path $Logs 'drain.pid') -Value $PID -Encoding ascii
Log "drain start pid=$PID slots=$(Slots)"

while ($true) {
    try {
        # Reap: a claimed request whose worker is gone and that has no result.
        foreach ($r in (Get-ChildItem -LiteralPath $Running -Filter '*.request' -ErrorAction SilentlyContinue)) {
            $name = $r.BaseName
            $pidFile = Join-Path $Running "$name.pid"
            $age = ((Get-Date) - $r.LastWriteTime).TotalSeconds
            $alive = $false
            if (Test-Path -LiteralPath $pidFile) {
                $wp = 0
                if ([int]::TryParse((Get-Content -LiteralPath $pidFile -Raw).Trim(), [ref]$wp)) {
                    $alive = ($null -ne (Get-Process -Id $wp -ErrorAction SilentlyContinue))
                }
            } elseif ($age -lt 90) { $alive = $true }   # worker still starting up
            if (-not $alive) {
                Log "reaping $name - worker died without a result"
                Move-Item -LiteralPath $r.FullName -Destination (Join-Path $Done "$name.request") -Force
                Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
                $res = @("STATUS: FAIL", "request: $name", "", "refused: the lane worker died before writing a result; re-queue as the next number")
                [IO.File]::WriteAllText((Join-Path $Done "$name.result"), (($res -join "`r`n") + "`r`n"), (New-Object Text.UTF8Encoding($false)))
            }
        }

        # A `pause` file stops new claims (running jobs finish); the orchestrator uses it for cache maintenance.
        if (Test-Path -LiteralPath (Join-Path $Lane 'pause')) { Start-Sleep -Seconds 5; continue }
        $inFlight = @(Get-ChildItem -LiteralPath $Running -Filter '*.request' -ErrorAction SilentlyContinue).Count
        $max = Slots
        if ($inFlight -lt $max) {
            # Builders write into their worktree's copy of this folder; scan those too.
            $queueDirs = @($Queue) + @(Get-ChildItem -Path (Join-Path $Root '.claude\worktrees\*\.claude\lane\queue') -Directory -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
            $waiting = @(Get-ChildItem -Path $queueDirs -Filter '*.request' -ErrorAction SilentlyContinue |
                Where-Object { ((Get-Date) - $_.LastWriteTime).TotalSeconds -ge 2 } |   # let the writer finish
                Sort-Object @{Expression = { Priority $_ } }, LastWriteTime)
            foreach ($req in $waiting) {
                if ($inFlight -ge $max) { break }
                $dest = Join-Path $Running $req.Name
                try { Move-Item -LiteralPath $req.FullName -Destination $dest -ErrorAction Stop } catch { continue }
                (Get-Item -LiteralPath $dest).LastWriteTime = Get-Date   # claim time, for the reaper
                Start-Process -FilePath 'powershell.exe' -WindowStyle Hidden -ArgumentList @(
                    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$Worker`"", '-Request', "`"$dest`"") | Out-Null
                $inFlight++
                Log "claimed $($req.BaseName) ($inFlight/$max running)"
            }
        }
    } catch {
        Log "drain error: $($_.Exception.Message)"
    }
    Start-Sleep -Seconds 5
}
