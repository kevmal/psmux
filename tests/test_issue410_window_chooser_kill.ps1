# Issue #410: choose-tree window picker (Ctrl+b w) must allow killing the
# highlighted window via `x`, mirroring the session picker (Ctrl+b s).
#
# Reproduction strategy (WriteConsoleInput keystroke injection into the attached
# client, Layer 3): the choose-tree overlay is NOT captured by capture-pane and
# is NOT reflected in dump-state, so we prove the picker is genuinely OPEN by the
# fact that it ABSORBS typed characters (they never reach the shell). Then we
# press `x` and assert the window count drops.
$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$psmuxDir = "$env:USERPROFILE\.psmux"
$injectorExe = "$env:TEMP\psmux_injector.exe"
$script:TestsPassed = 0
$script:TestsFailed = 0
function Write-Pass($m){ Write-Host "  [PASS] $m" -ForegroundColor Green; $script:TestsPassed++ }
function Write-Fail($m){ Write-Host "  [FAIL] $m" -ForegroundColor Red; $script:TestsFailed++ }
function Info($m){ Write-Host $m -ForegroundColor Cyan }

# Compile injector if missing
if (-not (Test-Path $injectorExe)) {
    $csc = "C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
    & $csc /nologo /optimize /out:$injectorExe (Join-Path (Split-Path $PSScriptRoot -Parent) "tests\injector.cs") 2>&1 | Out-Null
}

function New-Sess($name, $extraWindows) {
    & $PSMUX kill-session -t $name 2>&1 | Out-Null
    Start-Sleep -Milliseconds 400
    Remove-Item "$psmuxDir\$name.*" -Force -EA SilentlyContinue
    $p = Start-Process -FilePath $PSMUX -ArgumentList "new-session","-s",$name -PassThru
    Start-Sleep -Seconds 4
    & $PSMUX rename-window -t "${name}:0" alpha 2>&1 | Out-Null
    foreach ($w in $extraWindows) { & $PSMUX new-window -t $name -n $w 2>&1 | Out-Null }
    Start-Sleep -Seconds 1
    return $p
}
function WinCount($name){ (& $PSMUX display-message -t $name -p '#{session_windows}' 2>&1).Trim() }
function ActiveWin($name){ (& $PSMUX display-message -t $name -p '#{window_name}' 2>&1).Trim() }
function Cap($name){ (& $PSMUX capture-pane -t $name -p 2>&1 | Out-String) }
function KillSess($p, $name){ & $PSMUX kill-session -t $name 2>&1 | Out-Null; try { Stop-Process -Id $p.Id -Force -EA SilentlyContinue } catch {} }

Write-Host "`n=== Issue #410: window chooser `x` kill ===" -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# TEST 1: `x` on the highlighted window kills it (core fix)
# ---------------------------------------------------------------------------
Info "`n[Test 1] Ctrl+b w, then x -> kills highlighted window"
$S = "issue410_t1"
$p = New-Sess $S @("bravo","charlie")
$before = WinCount $S
# open chooser, prove it's open by absorption, then x
& $injectorExe $p.Id "^b{SLEEP:500}w{SLEEP:1200}"
Start-Sleep -Seconds 1
& $injectorExe $p.Id "MARKERabsorb{SLEEP:500}"
Start-Sleep -Milliseconds 800
$absorbed = -not ((Cap $S) -match "MARKERabsorb")
& $injectorExe $p.Id "x{SLEEP:1200}"
Start-Sleep -Seconds 1
$after = WinCount $S
& $injectorExe $p.Id "{ESC}{SLEEP:300}" 2>&1 | Out-Null
if ($absorbed) { Write-Pass "chooser is open (typed chars absorbed, not leaked to shell)" }
else { Write-Fail "chooser did not absorb input; picker may not be open" }
if ([int]$after -eq [int]$before - 1) { Write-Pass "x killed one window ($before -> $after)" }
else { Write-Fail "x did not kill a window ($before -> $after)" }
KillSess $p $S

# ---------------------------------------------------------------------------
# TEST 2: navigate (jj) then x kills the SELECTED window (cursor path)
# The tree interleaves pane rows under each window, so absolute digit rows are
# not 1:1 with windows; jj navigation is deterministic. jj from the initial
# highlight lands on the second window (bravo) for the alpha/bravo/charlie
# layout, so x must remove bravo specifically.
# ---------------------------------------------------------------------------
Info "`n[Test 2] Ctrl+b w, jj (select bravo), then x -> kills bravo"
$S = "issue410_t2"
$p = New-Sess $S @("bravo","charlie")
& $PSMUX select-window -t "${S}:0" 2>&1 | Out-Null  # active = alpha so jj walks down into window rows
Start-Sleep -Milliseconds 600
$before = WinCount $S
& $injectorExe $p.Id "^b{SLEEP:500}w{SLEEP:1200}"
Start-Sleep -Seconds 1
& $injectorExe $p.Id "j{SLEEP:350}j{SLEEP:500}x{SLEEP:1500}"
Start-Sleep -Seconds 1
$after = WinCount $S
$names = (& $PSMUX list-windows -t $S -F '#{window_name}' 2>&1) -join ","
& $injectorExe $p.Id "{ESC}{SLEEP:300}" 2>&1 | Out-Null
if ([int]$after -eq [int]$before - 1 -and $names -notmatch "bravo") { Write-Pass "jj+x killed the selected window bravo ($before -> $after; remaining: $names)" }
else { Write-Fail "jj+x did not kill bravo as expected ($before -> $after; remaining: $names)" }
KillSess $p $S

# ---------------------------------------------------------------------------
# TEST 2b: digit-jump + x honours the jump buffer (kills the numbered window).
# For the alpha/bravo/charlie layout, displayed row 4 is the bravo window row
# (rows: 1=session header, 2=alpha, 3=alpha's pane, 4=bravo). x on "4" removes
# bravo, proving x consumes the digit buffer exactly like Enter does.
# ---------------------------------------------------------------------------
Info "`n[Test 2b] Ctrl+b w, type 4 (bravo row), then x -> kills bravo"
$S = "issue410_t2b"
$p = New-Sess $S @("bravo","charlie")
$before = WinCount $S
& $injectorExe $p.Id "^b{SLEEP:500}w{SLEEP:1200}"
Start-Sleep -Seconds 1
& $injectorExe $p.Id "4{SLEEP:400}x{SLEEP:1200}"
Start-Sleep -Seconds 1
$after = WinCount $S
$names = (& $PSMUX list-windows -t $S -F '#{window_name}' 2>&1) -join ","
& $injectorExe $p.Id "{ESC}{SLEEP:300}" 2>&1 | Out-Null
if ([int]$after -eq [int]$before - 1 -and $names -notmatch "bravo") { Write-Pass "digit-jump 4 + x killed bravo ($before -> $after; remaining: $names)" }
else { Write-Fail "digit-jump 4 + x did not kill bravo ($before -> $after; remaining: $names)" }
KillSess $p $S

# ---------------------------------------------------------------------------
# TEST 3 (regression): Enter still switches window (chooser not broken)
# ---------------------------------------------------------------------------
Info "`n[Test 3] Regression: Enter still selects/switches a window"
$S = "issue410_t3"
$p = New-Sess $S @("bravo","charlie")
& $PSMUX select-window -t "${S}:0" 2>&1 | Out-Null  # active = alpha
Start-Sleep -Milliseconds 600
$activeBefore = ActiveWin $S
# open chooser, jj to the bravo window row, Enter -> should switch active to bravo
& $injectorExe $p.Id "^b{SLEEP:500}w{SLEEP:1200}"
Start-Sleep -Seconds 1
& $injectorExe $p.Id "jj{SLEEP:400}{ENTER}{SLEEP:800}"
Start-Sleep -Seconds 1
$activeAfter = ActiveWin $S
& $injectorExe $p.Id "{ESC}{SLEEP:300}" 2>&1 | Out-Null
if ($activeAfter -ne $activeBefore -and $activeAfter -ne "") { Write-Pass "Enter switched active window ($activeBefore -> $activeAfter)" }
else { Write-Fail "Enter did not switch window ($activeBefore -> $activeAfter)" }
KillSess $p $S

# ---------------------------------------------------------------------------
# TEST 4 (regression): Esc still closes chooser, no window killed
# ---------------------------------------------------------------------------
Info "`n[Test 4] Regression: Esc closes chooser without killing"
$S = "issue410_t4"
$p = New-Sess $S @("bravo","charlie")
$before = WinCount $S
& $injectorExe $p.Id "^b{SLEEP:500}w{SLEEP:1200}"
Start-Sleep -Seconds 1
& $injectorExe $p.Id "{ESC}{SLEEP:600}"
Start-Sleep -Seconds 1
# after Esc, typing should reach the shell again
& $PSMUX send-keys -t "${S}:0" "cls" Enter 2>&1 | Out-Null
Start-Sleep -Milliseconds 800
& $injectorExe $p.Id "REACHshell{SLEEP:600}"
Start-Sleep -Seconds 1
$after = WinCount $S
$reached = ((Cap $S) -match "REACHshell")
& $injectorExe $p.Id "{ESC}{SLEEP:200}" 2>&1 | Out-Null
if ([int]$after -eq [int]$before) { Write-Pass "Esc killed no windows ($before -> $after)" }
else { Write-Fail "window count changed after Esc ($before -> $after)" }
if ($reached) { Write-Pass "Esc closed chooser (input reaches shell again)" }
else { Write-Fail "input did not reach shell after Esc" }
KillSess $p $S

Write-Host "`n=== Results ===" -ForegroundColor Cyan
Write-Host "  Passed: $($script:TestsPassed)" -ForegroundColor Green
Write-Host "  Failed: $($script:TestsFailed)" -ForegroundColor $(if ($script:TestsFailed -gt 0) { "Red" } else { "Green" })
exit $script:TestsFailed
