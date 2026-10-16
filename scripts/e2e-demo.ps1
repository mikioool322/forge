# Forge end-to-end demo (Sprint 01).
#
# Runs a full lifecycle in an ISOLATED temporary workspace (never touches
# your application repositories):
#
#   doctor -> init -> daemon start -> plan (2 concurrent tasks + 1 dependent)
#   -> worktree/diff inspection -> logs -> cancellation -> TUI attach/detach
#   -> crash recovery -> daemon stop -> restart persistence
#
# Usage:
#   .\scripts\e2e-demo.ps1                 # uses target\release or target\debug
#   .\scripts\e2e-demo.ps1 -ForgeBin path  # explicit binary
#
# Exit code 0 = all checks passed.

param(
    [string]$ForgeBin = "",
    [string]$WorkRoot = "$env:TEMP\forge-e2e-demo"
)

$ErrorActionPreference = 'Continue'
$script:Failures = 0
$script:Checks = 0

function Step([string]$Name) {
    Write-Host ""
    Write-Host "== $Name ==" -ForegroundColor Cyan
}

function Check([string]$Name, [bool]$Ok, [string]$Detail = "") {
    $script:Checks++
    if ($Ok) {
        Write-Host ("  PASS  {0} {1}" -f $Name, $Detail) -ForegroundColor Green
    } else {
        $script:Failures++
        Write-Host ("  FAIL  {0} {1}" -f $Name, $Detail) -ForegroundColor Red
    }
}

function Wait-Until {
    param(
        [scriptblock]$Condition,
        [int]$TimeoutSeconds = 60,
        [string]$What = "condition"
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $value = & $Condition
        if ($value) { return $value }
        Start-Sleep -Milliseconds 400
    }
    throw "timeout waiting for $What"
}

function Get-StatusJson {
    $json = & $Forge --data-dir $DataDir --config $ConfigPath status --json
    return ($json | ConvertFrom-Json)
}

if (-not $ForgeBin) {
    $candidates = @(
        (Join-Path $PSScriptRoot "..\target\release\forge.exe"),
        (Join-Path $PSScriptRoot "..\target\debug\forge.exe")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) { $ForgeBin = (Resolve-Path $candidate).Path; break }
    }
}
if (-not (Test-Path $ForgeBin)) {
    throw "forge binary not found; build it first (cargo build --release) or pass -ForgeBin"
}
$Forge = (Resolve-Path $ForgeBin).Path

Write-Host "forge binary: $ForgeBin"
Write-Host "workspace:    $WorkRoot"

# --- fresh isolated workspace ---------------------------------------------
Remove-Item -Recurse -Force $WorkRoot -ErrorAction SilentlyContinue
$Repo = Join-Path $WorkRoot "repo"
$DataDir = Join-Path $WorkRoot "data"
$CfgDir = Join-Path $WorkRoot "cfg"
New-Item -ItemType Directory -Force -Path $Repo, $DataDir, $CfgDir | Out-Null

Set-Location $Repo
git init -b main | Out-Null
git config user.name "forge-e2e"
git config user.email "forge-e2e@test"
Set-Content -Path (Join-Path $Repo "README.md") -Value "forge e2e demo repository"
git add . | Out-Null
git commit -m "initial commit" | Out-Null

$ConfigPath = Join-Path $CfgDir "config.toml"
@'
[daemon]
max_concurrency = 2
task_timeout_secs = 90
heartbeat_interval_secs = 2
lease_timeout_secs = 30

[git]
branch_prefix = "forge/"

[logs]
max_file_bytes = 1048576

[[agents]]
id = "sim-a"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "sim-a", "--file", "alpha.txt", "--steps", "3", "--sleep-ms", "200"]
capabilities = ["code"]

[[agents]]
id = "sim-b"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "sim-b", "--file", "beta.txt", "--steps", "3", "--sleep-ms", "200"]
capabilities = ["code"]

[[agents]]
id = "sim-slow"
kind = "generic"
command = "@self"
extra_args = ["__demo-agent", "--name", "sim-slow", "--steps", "1", "--sleep-ms", "100", "--hang-ms", "45000"]
capabilities = ["slow"]
'@ | Set-Content -Path $ConfigPath

$PlanA = Join-Path $WorkRoot "plan-alpha-beta.toml"
@'
plan_version = 1
project = "demo-api"

[[tasks]]
key = "alpha"
title = "create alpha module"
spec = "Create alpha.txt."
capabilities = ["code"]
claims = ["file:alpha.txt"]

[[tasks]]
key = "beta"
title = "create beta module"
spec = "Create beta.txt."
capabilities = ["code"]
claims = ["file:beta.txt"]

[[tasks]]
key = "gamma"
title = "integration report"
spec = "Describe alpha and beta."
capabilities = ["code"]
deps = ["alpha", "beta"]
'@ | Set-Content -Path $PlanA

$PlanSlow = Join-Path $WorkRoot "plan-slow.toml"
@'
plan_version = 1
project = "demo-api"

[[tasks]]
key = "slow"
title = "long running task"
spec = "Sleep."
capabilities = ["slow"]
'@ | Set-Content -Path $PlanSlow

# Distinct content (different request id) for the crash-recovery step:
# re-running the same file would be an idempotent replay on purpose.
$PlanSlow2 = Join-Path $WorkRoot "plan-slow2.toml"
@'
plan_version = 1
project = "demo-api"

[[tasks]]
key = "slow2"
title = "long running task (recovery scenario)"
spec = "Sleep again."
capabilities = ["slow"]
'@ | Set-Content -Path $PlanSlow2

function Invoke-Forge {
    param([string[]]$Arguments)
    & $Forge --data-dir $DataDir --config $ConfigPath @Arguments
}

# ---------------------------------------------------------------------------
Step "forge doctor (before daemon)"
$doctorOutput = & $Forge --data-dir $DataDir --config $ConfigPath doctor
$doctorOutput | ForEach-Object { Write-Host "  $_" }
Check "doctor runs" ($LASTEXITCODE -eq 0)
Check "doctor reports git" (($doctorOutput -join "`n") -match "git\s+ok")

# ---------------------------------------------------------------------------
Step "forge init"
$initOutput = Invoke-Forge @("init", "--name", "demo-api")
$initOutput | ForEach-Object { Write-Host "  $_" }
Check "project registered" (($initOutput -join "`n") -match "demo-api \(registered\)")

# ---------------------------------------------------------------------------
Step "daemon start (background)"
$startOutput = Invoke-Forge @("daemon", "start", "--background")
$startOutput | ForEach-Object { Write-Host "  $_" }
$statusOutput = Invoke-Forge @("daemon", "status")
$statusOutput | ForEach-Object { Write-Host "  $_" }
Check "daemon running" (($statusOutput -join "`n") -match "daemon: running")

# ---------------------------------------------------------------------------
Step "submit plan (2 independent + 1 dependent task)"
$runOutput = Invoke-Forge @("run", $PlanA)
$runOutput | ForEach-Object { Write-Host "  $_" }
Check "plan accepted" (($runOutput -join "`n") -match "accepted")

$final = Wait-Until -TimeoutSeconds 90 -What "alpha/beta/gamma to complete" -Condition {
    $status = Get-StatusJson
    $states = @{}
    foreach ($t in $status.tasks) { $states[$t.task.seq_no] = $t.task.state }
    if ($states[1] -eq 'COMPLETED' -and $states[2] -eq 'COMPLETED' -and $states[3] -eq 'COMPLETED') {
        return $true
    }
    return $false
}
$status = Get-StatusJson
$status.tasks | ForEach-Object {
    Write-Host ("  {0}  {1,-10} agent={2,-9} {3}" -f $_.task.seq_no, $_.task.state, $_.task.agent_id, $_.task.title)
}
$bySeq = @{}
foreach ($t in $status.tasks) { $bySeq[$t.task.seq_no] = $t }
Check "T-001 completed (alpha)" ($bySeq[1].task.state -eq 'COMPLETED')
Check "T-002 completed (beta)"  ($bySeq[2].task.state -eq 'COMPLETED')
Check "T-003 completed (gamma, depended on both)" ($bySeq[3].task.state -eq 'COMPLETED')
Check "two different agents used" ($bySeq[1].task.agent_id -ne $bySeq[2].task.agent_id)

# concurrent execution evidence: started_at of alpha/beta overlap
$start1 = $bySeq[1].task.started_at
$start2 = $bySeq[2].task.started_at
Check "tasks overlapped in time" ([Math]::Abs($start1 - $start2) -lt 5000) "delta=$([Math]::Abs($start1 - $start2))ms"

# ---------------------------------------------------------------------------
Step "worktree isolation and diffs"
$WtRoot = Join-Path $DataDir "worktrees\demo-api"
$wtA = Join-Path $WtRoot "t-001-r0"
$wtB = Join-Path $WtRoot "t-002-r0"
$wtG = Join-Path $WtRoot "t-003-r0"
Check "worktree for T-001 exists" (Test-Path (Join-Path $wtA "alpha.txt"))
Check "worktree for T-002 exists" (Test-Path (Join-Path $wtB "beta.txt"))
Check "T-003 worktree exists" (Test-Path $wtG)
Check "worktrees are isolated" (-not (Test-Path (Join-Path $wtA "beta.txt")) -and -not (Test-Path (Join-Path $wtB "alpha.txt")))
$dirty = (git -C $Repo status --porcelain) -join ""
Check "canonical repository untouched" ([string]::IsNullOrWhiteSpace($dirty))
git -C $wtA add -A -N 2>$null | Out-Null
$diffStat = (git -C $wtA diff --stat) -join "`n"
Write-Host "  diff --stat (T-001 worktree):" -ForegroundColor DarkGray
$diffStat -split "`n" | ForEach-Object { Write-Host "    $_" }
Check "diff shows alpha.txt" ($diffStat -match "alpha.txt")

# ---------------------------------------------------------------------------
Step "logs"
$logs = Invoke-Forge @("logs", "T-001", "-n", "200")
$logsTail = ($logs | Select-Object -Last 6) -join "`n"
Write-Host $logsTail
Check "log contains agent output" (($logs -join "`n") -match "wrote alpha.txt")
Check "log contains process exit" (($logs -join "`n") -match "process exited")

# ---------------------------------------------------------------------------
Step "cancellation"
$runOutput = Invoke-Forge @("run", $PlanSlow)
$runOutput | ForEach-Object { Write-Host "  $_" }
Wait-Until -TimeoutSeconds 30 -What "slow task running" -Condition {
    $s = Get-StatusJson
    $t4 = $s.tasks | Where-Object { $_.task.seq_no -eq 4 }
    if ($t4 -and $t4.task.state -eq 'RUNNING') { return $true }
    return $false
} | Out-Null
$cancelOutput = Invoke-Forge @("cancel", "T-004")
$cancelOutput | ForEach-Object { Write-Host "  $_" }
$cancelled = Wait-Until -TimeoutSeconds 30 -What "cancelled state" -Condition {
    $s = Get-StatusJson
    $t4 = $s.tasks | Where-Object { $_.task.seq_no -eq 4 }
    if ($t4.task.state -eq 'CANCELLED' -and $t4.task.failure_reason -eq 'cancelled') { return $true }
    return $false
}
Check "cancelled task is CANCELLED" $true
Start-Sleep -Seconds 2
$s = Get-StatusJson
$t4 = $s.tasks | Where-Object { $_.task.seq_no -eq 4 }
Check "cancellation is stable" ($t4.task.state -eq 'CANCELLED')

# ---------------------------------------------------------------------------
Step "TUI attach / detach / reconnect"
$TuiOut = Join-Path $WorkRoot "tui1.out"
$tui = Start-Process -FilePath $Forge -ArgumentList @("--data-dir", $DataDir, "--config", $ConfigPath, "tui") -PassThru -RedirectStandardOutput $TuiOut -RedirectStandardError (Join-Path $WorkRoot "tui1.err") -WindowStyle Hidden
Start-Sleep -Seconds 4
$tuiAlive = [bool](Get-Process -Id $tui.Id -ErrorAction SilentlyContinue)
Check "TUI attached to the daemon" $tuiAlive
if ($tuiAlive) { Stop-Process -Id $tui.Id -Force }   # detach: the daemon keeps running
Start-Sleep -Milliseconds 800
$afterDetach = Invoke-Forge @("daemon", "status")
$afterDetach | ForEach-Object { Write-Host "  $_" }
Check "daemon survives TUI detach" (($afterDetach -join "`n") -match "daemon: running")

# reconnect: a second session sees the same state
$TuiOut2 = Join-Path $WorkRoot "tui2.out"
$tui2 = Start-Process -FilePath $Forge -ArgumentList @("--data-dir", $DataDir, "--config", $ConfigPath, "tui") -PassThru -RedirectStandardOutput $TuiOut2 -RedirectStandardError (Join-Path $WorkRoot "tui2.err") -WindowStyle Hidden
Start-Sleep -Seconds 4
$tui2Alive = [bool](Get-Process -Id $tui2.Id -ErrorAction SilentlyContinue)
if ($tui2Alive) { Stop-Process -Id $tui2.Id -Force }
Start-Sleep -Milliseconds 500
$tuiText = ""
if (Test-Path $TuiOut2) { $tuiText = Get-Content $TuiOut2 -Raw }
Check "TUI reconnects and renders task data" (($tuiText -match "attached to") -and ($tuiText -match "T-001"))

# ---------------------------------------------------------------------------
Step "crash recovery (kill -9 the daemon while a task runs)"
$runOutput = Invoke-Forge @("run", $PlanSlow2)
$runOutput | ForEach-Object { Write-Host "  $_" }
Wait-Until -TimeoutSeconds 30 -What "slow task running" -Condition {
    $s = Get-StatusJson
    return (@($s.tasks | Where-Object { $_.task.seq_no -eq 5 -and $_.task.state -eq 'RUNNING' }).Count -gt 0)
} | Out-Null
$pidInfo = & $Forge --data-dir $DataDir --config $ConfigPath daemon status | Select-String -Pattern "pid:"
$daemonPid = [int](($pidInfo -replace "[^0-9]", "").Trim())
Write-Host "  killing daemon pid $daemonPid (simulated crash)"
taskkill /PID $daemonPid /F /T 2>&1 | Out-Null
Start-Sleep -Seconds 1
$startOutput = Invoke-Forge @("daemon", "start", "--background")
$startOutput | ForEach-Object { Write-Host "  $_" }
$recovered = Wait-Until -TimeoutSeconds 30 -What "interrupted state after recovery" -Condition {
    $s = Get-StatusJson
    $t5 = $s.tasks | Where-Object { $_.task.seq_no -eq 5 }
    if ($t5.task.state -eq 'INTERRUPTED') { return $true }
    return $false
}
$s = Get-StatusJson
$t5 = $s.tasks | Where-Object { $_.task.seq_no -eq 5 }
Write-Host ("  T-005 state={0} reason={1}" -f $t5.task.state, $t5.task.failure_reason)
Check "task marked INTERRUPTED after crash" ($t5.task.state -eq 'INTERRUPTED')
$recoveryEvents = @($s.events | Where-Object { $_.kind -eq 'recovery' })
Check "recovery events recorded" ($recoveryEvents.Count -ge 1)

$retryOutput = Invoke-Forge @("retry", "T-005")
$retryOutput | ForEach-Object { Write-Host "  $_" }
Check "retry re-queues the task" (($retryOutput -join "`n") -match "re-queued")
$requeued = Wait-Until -TimeoutSeconds 40 -What "retried task to progress" -Condition {
    $s = Get-StatusJson
    $t5 = $s.tasks | Where-Object { $_.task.seq_no -eq 5 }
    return ($t5.task.state -eq 'RUNNING' -or $t5.task.state -eq 'COMPLETED' -or $t5.task.state -eq 'CANCELLED')
}
$s = Get-StatusJson
$t5 = $s.tasks | Where-Object { $_.task.seq_no -eq 5 }
Check "retried task dispatched again" ($t5.task.attempts -ge 1)
Invoke-Forge @("cancel", "T-005") | Out-Null
Start-Sleep -Seconds 1

# ---------------------------------------------------------------------------
Step "daemon stop and restart persistence"
$stopOutput = Invoke-Forge @("daemon", "stop")
$stopOutput | ForEach-Object { Write-Host "  $_" }
Check "endpoint file removed after stop" (-not (Test-Path (Join-Path $DataDir "run\endpoint")))
$startOutput = Invoke-Forge @("daemon", "start", "--background")
$startOutput | ForEach-Object { Write-Host "  $_" }
$s = Get-StatusJson
Check "tasks persisted across daemon restart" ($s.tasks.Count -ge 4)
Check "completed tasks still COMPLETED" (($s.tasks | Where-Object { $_.task.seq_no -eq 1 }).task.state -eq 'COMPLETED')
Invoke-Forge @("daemon", "stop") | Out-Null

# ---------------------------------------------------------------------------
Step "summary"
Write-Host ""
Write-Host ("{0} checks, {1} failed" -f $script:Checks, $script:Failures) -ForegroundColor ($(if ($script:Failures -eq 0) { "Green" } else { "Red" }))
Write-Host "workspace kept for inspection: $WorkRoot"
if ($script:Failures -gt 0) { exit 1 } else { exit 0 }
