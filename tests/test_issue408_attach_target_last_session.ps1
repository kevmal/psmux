# Issue #408 / discussion #430: attach-session -t <name> must honour the target
# instead of reattaching to the `last_session` marker.
#
# Root cause: the global arg scan strips -t out of cmd_args, so the attach branch
# re-parsed -t from the stripped args (found none) and fell through to
# resolve_last_session_name_ns. Whenever last_session pointed at a DIFFERENT
# session, `attach-session -t s2` silently landed on last_session (e.g. s1).
#
# Deterministic proof without an interactive TTY: launch the attach client with
# Start-Process, then ask the server (via list-clients) which session the client
# actually joined. list-clients prints "<pts>: <session>: ...".

$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$psmuxDir = "$env:USERPROFILE\.psmux"
$lastSessionFile = Join-Path $psmuxDir "last_session"
$script:TestsPassed = 0
$script:TestsFailed = 0

function Write-Pass($msg) { Write-Host "  [PASS] $msg" -ForegroundColor Green; $script:TestsPassed++ }
function Write-Fail($msg) { Write-Host "  [FAIL] $msg" -ForegroundColor Red; $script:TestsFailed++ }

function Reset-Server {
    & $PSMUX kill-server 2>&1 | Out-Null
    Get-Process psmux -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue
    Start-Sleep -Milliseconds 700
}

# Returns the session name that the newest attach client is bound to, or "" if none.
function Get-AttachedSession {
    param([string[]]$ArgList, [string]$ForceLastSession)
    if ($ForceLastSession) { Set-Content $lastSessionFile -Value $ForceLastSession -NoNewline }
    $proc = Start-Process -FilePath $PSMUX -ArgumentList $ArgList -WindowStyle Minimized -PassThru
    Start-Sleep -Seconds 2
    $clients = & $PSMUX list-clients 2>&1 | Out-String
    try { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue } catch {}
    Start-Sleep -Milliseconds 400
    # Parse the most recently active client line: "/dev/pts/N: <session>: cmd ..."
    $line = ($clients -split "`n" | Where-Object { $_ -match ':\s*\S+:\s' } | Select-Object -Last 1)
    if ($line -match '^\S+:\s*(\S+):') { return $Matches[1] }
    return ""
}

function New-TwoDetachedSessions {
    Reset-Server
    & $PSMUX new-session -d -s s1 -- cmd.exe; Start-Sleep -Milliseconds 600
    & $PSMUX new-session -d -s s2 -- cmd.exe; Start-Sleep -Milliseconds 600
}

Write-Host "`n=== Issue #408 attach target regression ===" -ForegroundColor Cyan

# --- Test 1: subcommand -t, last_session points elsewhere (the reported bug) ---
Write-Host "`n[Test 1] attach-session -t s2 with last_session=s1" -ForegroundColor Yellow
New-TwoDetachedSessions
$landed = Get-AttachedSession -ArgList @('attach-session','-t','s2') -ForceLastSession 's1'
if ($landed -eq 's2') { Write-Pass "Client attached to s2 (target honoured)" }
else { Write-Fail "Client attached to '$landed', expected s2 (BUG: last_session won)" }

# --- Test 2: global -t placement, last_session points elsewhere ---
Write-Host "`n[Test 2] -t s2 attach-session with last_session=s1" -ForegroundColor Yellow
New-TwoDetachedSessions
$landed = Get-AttachedSession -ArgList @('-t','s2','attach-session') -ForceLastSession 's1'
if ($landed -eq 's2') { Write-Pass "Client attached to s2 (global -t honoured)" }
else { Write-Fail "Client attached to '$landed', expected s2" }

# --- Test 3: symmetric - target s1 while last_session=s2 ---
Write-Host "`n[Test 3] attach-session -t s1 with last_session=s2" -ForegroundColor Yellow
New-TwoDetachedSessions
$landed = Get-AttachedSession -ArgList @('attach-session','-t','s1') -ForceLastSession 's2'
if ($landed -eq 's1') { Write-Pass "Client attached to s1 (target honoured)" }
else { Write-Fail "Client attached to '$landed', expected s1" }

# --- Test 4: bare `attach` (no -t) still falls back to last_session (tmux parity) ---
Write-Host "`n[Test 4] bare attach with last_session=s1 falls back to s1" -ForegroundColor Yellow
New-TwoDetachedSessions
$landed = Get-AttachedSession -ArgList @('attach') -ForceLastSession 's1'
if ($landed -eq 's1') { Write-Pass "Bare attach used last_session=s1 (fallback intact)" }
else { Write-Fail "Bare attach landed on '$landed', expected s1" }

# --- Test 5: positional target `attach s2` still works (no regression) ---
Write-Host "`n[Test 5] positional 'attach s2' with last_session=s1" -ForegroundColor Yellow
New-TwoDetachedSessions
$landed = Get-AttachedSession -ArgList @('attach','s2') -ForceLastSession 's1'
if ($landed -eq 's2') { Write-Pass "Positional attach to s2 honoured" }
else { Write-Fail "Positional attach landed on '$landed', expected s2" }

# --- Teardown ---
Reset-Server

Write-Host "`n=== Results ===" -ForegroundColor Cyan
Write-Host "  Passed: $($script:TestsPassed)" -ForegroundColor Green
Write-Host "  Failed: $($script:TestsFailed)" -ForegroundColor $(if ($script:TestsFailed -gt 0) { "Red" } else { "Green" })
exit $script:TestsFailed
