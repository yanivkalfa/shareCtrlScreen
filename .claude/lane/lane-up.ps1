# lane-up.ps1 - make sure exactly one drain is running. Safe to call any time.
$Lane = Split-Path -Parent $MyInvocation.MyCommand.Path
$Logs = Join-Path $Lane 'logs'
if (-not (Test-Path -LiteralPath $Logs)) { New-Item -ItemType Directory -Path $Logs | Out-Null }
$pidFile = Join-Path $Logs 'drain.pid'
if (Test-Path -LiteralPath $pidFile) {
    $dp = 0
    if ([int]::TryParse((Get-Content -LiteralPath $pidFile -Raw).Trim(), [ref]$dp)) {
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId = $dp" -ErrorAction SilentlyContinue
        if ($proc -and $proc.CommandLine -like '*drain.ps1*') { Write-Output "drain already running (pid $dp)"; exit 0 }
    }
}
$p = Start-Process -FilePath 'powershell.exe' -WindowStyle Hidden -PassThru -ArgumentList @(
    '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$(Join-Path $Lane 'drain.ps1')`"")
Write-Output "drain started (pid $($p.Id))"
