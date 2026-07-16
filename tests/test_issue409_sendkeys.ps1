# Issue #409 companion / regression guard for the send-keys route.
#
# IMPORTANT: `send-keys` modified-Enter is a SEPARATE code path from the
# interactive keypress fix.  send-keys is dispatched server-side
# (server/mod.rs -> parse_modified_special_key) and has always emitted xterm
# CSI 13;N~ for every modified Enter (Ctrl, Shift, Alt), independent of the
# interactive path's native injection.  The #409 fix targets the INTERACTIVE
# path (a user pressing Ctrl+Enter); see test_issue409_ctrl_enter.ps1 for that
# proof.  This test locks in the send-keys route so the interactive fix does
# NOT accidentally change scripted send-keys behavior.
#   send-keys Enter    -> 0x0D (CR)            plain Enter
#   send-keys C-Enter  -> CSI 13;5~            server-side xterm encoding (unchanged)
#   send-keys S-Enter  -> CSI 13;2~            server-side xterm encoding (unchanged)
#   send-keys M-Enter  -> CSI 13;3~            server-side xterm encoding (unchanged)
$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$SESSION = "issue409sk"
$psmuxDir = "$env:USERPROFILE\.psmux"
$recv = "$env:TEMP\psmux_409sk_recv.log"
$recvJs = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "ctrl_enter_recv.js"
$pass = 0; $fail = 0
function P($m){ Write-Host "  [PASS] $m" -ForegroundColor Green; $script:pass++ }
function F($m){ Write-Host "  [FAIL] $m" -ForegroundColor Red; $script:fail++ }

& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
Start-Sleep -Milliseconds 500
Remove-Item "$psmuxDir\$SESSION.*",$recv -Force -EA SilentlyContinue

$proc = Start-Process -FilePath $PSMUX -ArgumentList "new-session","-s",$SESSION -PassThru
Start-Sleep -Seconds 4
& $PSMUX send-keys -t $SESSION "node `"$recvJs`" `"$recv`"" Enter 2>&1 | Out-Null
$ready=$false; for($i=0;$i -lt 40;$i++){ Start-Sleep -Milliseconds 250; if((Test-Path $recv) -and ((Get-Content $recv -Raw) -match "READY")){$ready=$true;break} }
if(-not $ready){ F "receiver not ready"; & $PSMUX kill-session -t $SESSION 2>&1|Out-Null; exit 1 }

function SendKeyCheck($key,$expectHex,$label){
    $before = @(Get-Content $recv | Where-Object { $_ -like "BYTES:*" }).Count
    & $PSMUX send-keys -t $SESSION $key 2>&1 | Out-Null
    Start-Sleep -Milliseconds 700
    $lines = @(Get-Content $recv | Where-Object { $_ -like "BYTES:*" })
    $new = if($lines.Count -gt $before){ ($lines[$before..($lines.Count-1)] -join " ") } else { "(none)" }
    if($new -match $expectHex){ P "send-keys $label -> $new (expected $expectHex)" }
    else { F "send-keys $label -> $new (expected $expectHex)" }
}

Write-Host "`n=== #409 send-keys path (server-side, separate from interactive fix) ===" -ForegroundColor Cyan
SendKeyCheck "Enter"   "0d"                      "Enter"
SendKeyCheck "C-Enter" "1b 5b 31 33 3b 35 7e"    "C-Enter (CSI 13;5~)"
SendKeyCheck "S-Enter" "1b 5b 31 33 3b 32 7e"    "S-Enter (CSI 13;2~)"
SendKeyCheck "M-Enter" "1b 5b 31 33 3b 33 7e"    "M-Enter (CSI 13;3~)"

& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
try { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue } catch {}
Remove-Item "$psmuxDir\$SESSION.*" -Force -EA SilentlyContinue
Write-Host "`nPassed: $pass  Failed: $fail" -ForegroundColor $(if($fail){"Red"}else{"Green"})
Write-Host "Full log:" -ForegroundColor DarkGray
Get-Content $recv | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
exit $fail
