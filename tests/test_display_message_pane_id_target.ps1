# Regression test for pane-id targets in display-message.
#
# Verifies that a session-qualified pane-id target like "session:%1" resolves
# to that pane, even when another window is currently active. Also covers the
# compound "@window.%pane" form documented by psmux compatibility notes.

$ErrorActionPreference = "Stop"
$PSMUX = (Resolve-Path "$PSScriptRoot\..\target\debug\tmux.exe" -ErrorAction SilentlyContinue).Path
if (-not $PSMUX) { $PSMUX = (Resolve-Path "$PSScriptRoot\..\target\debug\psmux.exe" -ErrorAction SilentlyContinue).Path }
if (-not $PSMUX) { $PSMUX = (Resolve-Path "$PSScriptRoot\..\target\release\tmux.exe" -ErrorAction SilentlyContinue).Path }
if (-not $PSMUX) { $PSMUX = (Resolve-Path "$PSScriptRoot\..\target\release\psmux.exe" -ErrorAction SilentlyContinue).Path }
if (-not $PSMUX) { Write-Error "psmux/tmux binary not found"; exit 1 }

$oldPsmuxSession = $env:PSMUX_SESSION
$oldTmux = $env:TMUX
$oldNoWarm = $env:PSMUX_NO_WARM
$env:PSMUX_SESSION = $null
$env:TMUX = $null
$env:PSMUX_NO_WARM = "1"

$session = "dm_pane_id_" + [Guid]::NewGuid().ToString("N").Substring(0, 6)

function Assert-StartsWith {
    param([string]$Name, [string]$Actual, [string]$ExpectedPrefix)
    if (-not $Actual.StartsWith($ExpectedPrefix)) {
        throw "$Name failed: expected prefix '$ExpectedPrefix', got '$Actual'"
    }
    Write-Host "PASS: $Name"
}

try {
    & $PSMUX kill-session -t $session 2>$null | Out-Null
    & $PSMUX new-session -d -s $session | Out-Null
    Start-Sleep -Milliseconds 1200

    & $PSMUX new-window -t $session -n second | Out-Null
    Start-Sleep -Milliseconds 1200

    $rows = @(& $PSMUX list-panes -a -t $session -F '#{pane_id} #{window_id} #{window_index}' | Where-Object { $_.Trim() })
    if ($rows.Count -lt 2) {
        throw "expected at least two panes, got: $($rows -join '; ')"
    }

    $target = $rows[0] -split ' '
    $active = $rows[$rows.Count - 1] -split ' '
    $targetPane = $target[0]
    $targetWindow = $target[1]
    $activePane = $active[0]

    $activeNow = (& $PSMUX display-message -t $session -p '#{pane_id} #{window_id} #{window_index}' | Out-String).Trim()
    Assert-StartsWith "setup active pane" $activeNow "$activePane "

    $byPane = (& $PSMUX display-message -t "$session`:$targetPane" -p '#{pane_id} #{window_id} #{window_index}' | Out-String).Trim()
    Assert-StartsWith "display-message session:%pane" $byPane "$targetPane "

    $byCompound = (& $PSMUX display-message -t "$session`:$targetWindow.$targetPane" -p '#{pane_id} #{window_id} #{window_index}' | Out-String).Trim()
    Assert-StartsWith "display-message session:@window.%pane" $byCompound "$targetPane "
}
finally {
    & $PSMUX kill-session -t $session 2>$null | Out-Null
    $env:PSMUX_SESSION = $oldPsmuxSession
    $env:TMUX = $oldTmux
    $env:PSMUX_NO_WARM = $oldNoWarm
}
