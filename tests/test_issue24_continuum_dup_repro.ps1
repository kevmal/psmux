# Issue #24 (psmux-plugins): psmux-continuum auto_save.ps1 spawns a duplicate
# background process on EVERY client attach, with no single-instance guard.
#
# This test REPRODUCES the bug tangibly: it sources the real continuum
# plugin.conf into a live psmux session, attaches/detaches a client N times via
# Win32, and counts the pwsh processes running auto_save.ps1 after each attach.
# If the count grows by one per attach, the bug is proven.

$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$SESSION = "issue24_repro"
$psmuxDir = "$env:USERPROFILE\.psmux"
$PLUGINCONF = "$psmuxDir\plugins\psmux-continuum\plugin.conf"
$script:Pass = 0; $script:Fail = 0
function Write-Pass($m){ Write-Host "  [PASS] $m" -ForegroundColor Green; $script:Pass++ }
function Write-Fail($m){ Write-Host "  [FAIL] $m" -ForegroundColor Red; $script:Fail++ }

function Count-AutoSave {
    @(Get-CimInstance Win32_Process -Filter "Name='pwsh.exe'" -EA SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine -like '*auto_save*' }).Count
}
function Kill-AutoSave {
    Get-CimInstance Win32_Process -Filter "Name='pwsh.exe'" -EA SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine -like '*auto_save*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -EA SilentlyContinue }
}

function Cleanup {
    & $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
    Start-Sleep -Milliseconds 500
    Kill-AutoSave
    Remove-Item "$psmuxDir\$SESSION.*" -Force -EA SilentlyContinue
}

Write-Host "=== Issue #24 reproduction: duplicate auto_save processes per attach ===" -ForegroundColor Cyan
if (-not (Test-Path $PLUGINCONF)) { Write-Fail "plugin.conf not found at $PLUGINCONF"; exit 1 }

Cleanup
# Use a tiny interval so the script reaches its first save quickly (not required for the
# count, but proves the loop is alive). The bug is about process accumulation regardless.
& $PSMUX new-session -d -s $SESSION
Start-Sleep -Seconds 3
& $PSMUX has-session -t $SESSION 2>$null
if ($LASTEXITCODE -ne 0) { Write-Fail "session create failed"; exit 1 }

# source the real continuum plugin.conf -> registers the client-attached hook
& $PSMUX source-file -t $SESSION $PLUGINCONF 2>&1 | Out-Null
Start-Sleep -Milliseconds 500

$base = Count-AutoSave
Write-Host "  baseline auto_save processes (no client attached): $base"

$counts = @()
$ATTACHES = 4
for ($i = 1; $i -le $ATTACHES; $i++) {
    # attach a client (fires client-attached -> run-shell auto_save.ps1, server-side)
    $proc = Start-Process -FilePath $PSMUX -ArgumentList "attach","-t",$SESSION -PassThru -WindowStyle Minimized
    Start-Sleep -Seconds 3   # let the hook fire and pwsh spawn
    $c = Count-AutoSave
    $counts += $c
    Write-Host "  after attach #$i : auto_save processes = $c"
    # detach by killing the client process (and any conhost child)
    try { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue } catch {}
    Get-CimInstance Win32_Process -Filter "Name='psmux.exe'" -EA SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine -like '*attach*' -and $_.CommandLine -like "*$SESSION*" } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -EA SilentlyContinue }
    Start-Sleep -Seconds 2
    $after = Count-AutoSave
    Write-Host "    after detach #$i : auto_save processes = $after (should persist if bug present)"
}

$final = Count-AutoSave
Write-Host "`n  final auto_save process count: $final after $ATTACHES attaches" -ForegroundColor Yellow

# BUG is reproduced if the count grew with attaches (more than 1 surviving process)
if ($final -ge 2 -and $counts[-1] -gt $counts[0]) {
    Write-Pass "BUG REPRODUCED: auto_save processes accumulate per attach (final=$final, progression: $($counts -join ','))"
} elseif ($final -le 1) {
    Write-Fail "Not reproduced: count did not accumulate (final=$final). Either already fixed or attach did not fire the hook."
} else {
    Write-Fail "Inconclusive: final=$final progression=$($counts -join ',')"
}

Cleanup
Write-Host "`n=== Results: Passed=$($script:Pass) Failed=$($script:Fail) ===" -ForegroundColor Cyan
exit $script:Fail
