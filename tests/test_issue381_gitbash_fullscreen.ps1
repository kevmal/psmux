# Issue #381: git bash prints raw mouse escape sequences after commands fill the screen.
#
# The root cause (is_fullscreen_tui false-positive on a filled shell screen,
# which forwards mouse motion to the shell) and the fix are proven irrefutably
# by tests-rs/test_issue381_gitbash_fullscreen_falsepositive.rs, which drives the
# REAL is_fullscreen_tui / pane_wants_mouse / foreground_is_shell functions with
# LIVE processes (real git bash -> Some(true), real non-shell -> Some(false)).
#
# This is the mandatory real-psmux layer: launch an ACTUAL git bash session,
# fill the screen exactly as the reporter describes, and confirm the session
# stays a healthy interactive bash shell (foreground == bash, pane not dead, no
# crash). Note: the git bash prompt does not always render into capture-pane
# under a programmatic (non-terminal) launch because MSYS/ConPTY handles the tty
# differently, so this layer asserts on the robust process-level signals; the
# byte-level routing proof lives in the Rust suite.

$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$SESSION = "test_issue381_gb"
$psmuxDir = "$env:USERPROFILE\.psmux"
$GITBASH = "C:\Program Files\Git\bin\bash.exe"
$CONF = "$env:TEMP\psmux_issue381_gb.conf"
$script:TestsPassed = 0
$script:TestsFailed = 0
function Write-Pass($m) { Write-Host "  [PASS] $m" -ForegroundColor Green; $script:TestsPassed++ }
function Write-Fail($m) { Write-Host "  [FAIL] $m" -ForegroundColor Red; $script:TestsFailed++ }

if (-not (Test-Path $GITBASH)) { Write-Host "git bash not found; skipping" -ForegroundColor Yellow; exit 0 }

# default-shell must use forward slashes; a space-containing positional command
# arg to new-session is not parsed as the shell.
"set -g default-shell `"C:/Program Files/Git/bin/bash.exe`"`nset -g mouse on" | Set-Content -Path $CONF -Encoding UTF8

& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
Start-Sleep -Milliseconds 500

Write-Host "`n=== Issue #381 git bash E2E ===" -ForegroundColor Cyan

$env:PSMUX_CONFIG_FILE = $CONF
$proc = Start-Process -FilePath $PSMUX -ArgumentList "new-session","-s",$SESSION -PassThru
$ok = $false
for ($i = 0; $i -lt 40; $i++) { Start-Sleep -Milliseconds 500; if (Test-Path "$psmuxDir\$SESSION.port") { $ok = $true; break } }
$env:PSMUX_CONFIG_FILE = $null

if (-not $ok) { Write-Fail "git bash session did not start"; exit 1 }
Write-Pass "git bash session started (visible TUI window)"
Start-Sleep -Seconds 3

# [Test 1] Foreground process is bash (the exact signal the fix keys on).
$cmd = (& $PSMUX display-message -t $SESSION -p '#{pane_current_command}' 2>&1 | Out-String).Trim()
Write-Host "  pane_current_command = '$cmd'" -ForegroundColor DarkGray
if ($cmd -match "bash") { Write-Pass "foreground process is bash -> foreground_is_shell path active" }
else { Write-Fail "expected bash foreground, got '$cmd'" }

# [Test 2] Fill the screen the way the reporter does (enough output to fill it).
# Single-quoted so PowerShell passes the $(...) subshell to bash verbatim.
& $PSMUX send-keys -t $SESSION 'for i in $(seq 1 80); do echo FILL_$i; done' Enter 2>&1 | Out-Null
Start-Sleep -Seconds 3

# [Test 3] Session survives the fill (no crash) and pane is not dead.
& $PSMUX has-session -t $SESSION 2>$null
if ($LASTEXITCODE -eq 0) { Write-Pass "session alive after screen fill" }
else { Write-Fail "session died after screen fill" }

$dead = (& $PSMUX display-message -t $SESSION -p '#{pane_dead}' 2>&1 | Out-String).Trim()
if ($dead -eq "0") { Write-Pass "pane not dead after screen fill" }
else { Write-Fail "pane_dead=$dead after screen fill" }

$cmd2 = (& $PSMUX display-message -t $SESSION -p '#{pane_current_command}' 2>&1 | Out-String).Trim()
if ($cmd2 -match "bash") { Write-Pass "foreground still bash after fill (fix's classifier still applies)" }
else { Write-Fail "foreground changed to '$cmd2' after fill" }

& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
try { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue } catch {}
Remove-Item $CONF -Force -EA SilentlyContinue

Write-Host "`n=== Results ===" -ForegroundColor Cyan
Write-Host "  Passed: $($script:TestsPassed)" -ForegroundColor Green
Write-Host "  Failed: $($script:TestsFailed)" -ForegroundColor $(if ($script:TestsFailed -gt 0) { "Red" } else { "Green" })
exit $script:TestsFailed
