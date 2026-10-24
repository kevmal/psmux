# Every mutation uses a fresh namespace and this worktree's binary. No installed
# binary fallback, deployment, default-server mutation, or kill-server cleanup.
param([string]$Binary = "$PSScriptRoot/../target/debug/psmux.exe")
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Namespace = 'window-target-' + [guid]::NewGuid().ToString('N')
$Session = 'regression'
$Config = Join-Path (Split-Path $Binary) "$Namespace.conf"
$SavedWarm = $env:PSMUX_NO_WARM
$DefaultBefore = @(& $Binary list-sessions -F '#{session_name}' 2>$null | Sort-Object)
$SnapshotPrefix = Join-Path (Split-Path $Binary) $Namespace
$DefaultBefore | Set-Content -LiteralPath "$SnapshotPrefix-default-before.txt"
$script:Checks = 0
function Px([string[]]$Command) {
    $output = (& $Binary -L $Namespace @Command 2>&1 | Out-String).Trim()
    [pscustomobject]@{ Code = $LASTEXITCODE; Output = $output }
}
function Check($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
    $script:Checks++
    Write-Host "PASS: $Message"
}
function Windows {
    $result = Px -Command @('list-windows', '-t', $Session, '-F', '#{window_id}:#{window_name}:#{window_active}:#{window_layout}:#{window_width}x#{window_height}')
    if ($result.Code -ne 0) { throw $result.Output }
    $result.Output
}
function State {
    $windows = Windows
    $panes = Px -Command @('list-panes', '-s', '-t', $Session, '-F', '#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}')
    if ($panes.Code -ne 0) { throw $panes.Output }
    $windows + "`n" + $panes.Output
}
function Tcp([string]$Command, [switch]$Control) {
    # Connect directly to THIS namespace/session, bypassing CLI validation.
    $base = Join-Path $env:USERPROFILE ".psmux/$($Namespace)__$Session"
    $port = [int](Get-Content -LiteralPath "$base.port" -Raw).Trim()
    $key = (Get-Content -LiteralPath "$base.key" -Raw).Trim()
    $client = [Net.Sockets.TcpClient]::new('127.0.0.1', $port)
    try {
        $stream = $client.GetStream()
        $stream.ReadTimeout = 10000
        $writer = [IO.StreamWriter]::new($stream)
        $reader = [IO.StreamReader]::new($stream)
        $writer.WriteLine("AUTH $key"); $writer.Flush()
        $null = $reader.ReadLine()
        if ($Control) {
            $writer.WriteLine('CONTROL_NOECHO'); $writer.Flush()
            # Initial control-mode ready block.
            do { $line = $reader.ReadLine() } while ($null -ne $line -and $line -notlike '%end *')
            $writer.WriteLine($Command); $writer.Flush()
            $lines = [Collections.Generic.List[string]]::new()
            do {
                $line = $reader.ReadLine()
                if ($null -eq $line) { break }
                $lines.Add($line)
            } while ($line -notlike '%end *' -and $line -notlike '%error *')
            return $lines -join "`n"
        }
        $writer.WriteLine($Command); $writer.Flush()
        $client.Client.Shutdown([Net.Sockets.SocketShutdown]::Send)
        $reader.ReadToEnd().Trim()
    } finally { $client.Dispose() }
}
try {
    $env:PSMUX_NO_WARM = '1'
    [IO.File]::WriteAllText($Config, '')
    $setup = (& $Binary -L $Namespace -f $Config new-session -d -s $Session -x 100 -y 30 'cmd.exe /k' 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw $setup }
    foreach ($name in @('victim', 'survivor')) {
        $r = Px -Command @('new-window', '-d', '-t', $Session, '-n', $name, 'cmd.exe /k')
        if ($r.Code -ne 0) { throw $r.Output }
    }
    $panes = Px -Command @('list-panes', '-s', '-t', $Session, '-F', '#{window_name}|#{window_id}|#{pane_id}')
    $victim = ($panes.Output -split "`r?`n" | Where-Object { $_ -like 'victim|*' }) -split '\|'
    $survivor = ($panes.Output -split "`r?`n" | Where-Object { $_ -like 'survivor|*' }) -split '\|'
    Check ($victim.Count -eq 3 -and $survivor.Count -eq 3) 'setup has separate victim and survivor panes'
    $null = Px -Command @('select-window', '-t', "${Session}:$($survivor[1])")
    $r = Px -Command @('kill-window', '-t', "${Session}:$($victim[2])")
    Check ($r.Code -eq 0) 'session:%pane kill succeeds'
    $after = Windows
    Check ($after -notmatch [regex]::Escape($victim[1] + ':')) 'pane target killed its own window'
    Check ($after -match [regex]::Escape($survivor[1] + ':survivor')) 'active survivor window remains alive'

    $r = Px -Command @('rename-window', "--target=${Session}:$($survivor[2])", 'renamed-survivor')
    Check ($r.Code -eq 0 -and (Windows) -match [regex]::Escape($survivor[1] + ':renamed-survivor:1')) 'rename-window resolves a pane target'
    $null = Px -Command @('rename-window', '-t', "${Session}:$($survivor[2])", 'survivor')
    foreach ($command in @('kill-pane', 'unlink-window')) {
        $r = Px -Command @('new-window', '-d', '-t', $Session, '-n', 'other-victim', 'cmd.exe /k')
        if ($r.Code -ne 0) { throw $r.Output }
        $panes = Px -Command @('list-panes', '-s', '-t', $Session, '-F', '#{window_name}|#{window_id}|#{pane_id}')
        $victim = ($panes.Output -split "`r?`n" | Where-Object { $_ -like 'other-victim|*' }) -split '\|'
        $r = Px -Command @('select-window', '-t', "${Session}:$($victim[2])")
        Check ($r.Code -eq 0 -and (Windows) -match [regex]::Escape($victim[1] + ':other-victim:1')) 'select-window resolves the containing window of a pane'
        $null = Px -Command @('select-window', '-t', "${Session}:$($survivor[2])")
        $r = Px -Command @($command, '-t', "${Session}:.%$($victim[2].TrimStart('%'))")
        Check ($r.Code -eq 0 -and (Windows) -notmatch [regex]::Escape($victim[1] + ':')) "$command resolves the pane's own window"
        Check ((Windows) -match [regex]::Escape($survivor[1] + ':survivor:1')) "$command preserves the active survivor"
    }

    foreach ($target in @("${Session}:%999999", "${Session}:%bad", "${Session}:@bad", "${Session}:.%bad", "${Session}:missing-window")) {
        foreach ($command in @('kill-window', 'kill-pane', 'select-window', 'rename-window', 'respawn-window', 'unlink-window', 'resize-window', 'rotate-window', 'select-layout', 'next-layout', 'previous-layout', 'set-window-option')) {
            $before = State
            $cmdArgs = @($command, '-t', $target)
            if ($command -eq 'rename-window') { $cmdArgs += 'WRONG' }
            if ($command -eq 'resize-window') { $cmdArgs += @('-x', '101') }
            if ($command -eq 'set-window-option') { $cmdArgs += @('@window-target-probe', 'WRONG') }
            $r = Px -Command $cmdArgs
            Check ($r.Code -ne 0) "$command rejects $target with a nonzero exit"
            Check ((State) -eq $before) "$command preserves windows, focus, panes and processes on a missing target"
        }
    }
    foreach ($command in @('move-window', 'swap-window', 'link-window')) {
        $before = State
        $r = Px -Command @($command, '-s', "${Session}:%bad", '-t', "${Session}:9")
        Check ($r.Code -ne 0) "$command rejects a malformed source"
        Check ((State) -eq $before) "$command cannot substitute the active source"
    }
    foreach ($targetFlags in @(
        "-t${Session}:%bad", "-t=${Session}:%bad", "--target ${Session}:%bad", "--target=${Session}:%bad",
        "-at ${Session}:%bad", "-at${Session}:%bad", "-at=${Session}:%bad",
        "-t${Session}:%999999", '-t', '-at', '--target', '--target=', '-t=', '-1tbad', '-_tbad', '-Ztbad')) {
        $before = State
        $r = Px -Command (@('kill-window') + ($targetFlags -split ' '))
        Check ($r.Code -ne 0) "CLI rejects explicit target spelling $targetFlags"
        Check ((State) -eq $before) 'CLI target spelling cannot kill the active window'
    }
    foreach ($control in @($false, $true)) {
        foreach ($targetFlags in @(
            "-t${Session}:%bad", "-t=${Session}:%bad", "--target ${Session}:%bad", "--target=${Session}:%bad",
            "-at ${Session}:%bad", "-at${Session}:%bad", "-at=${Session}:%bad",
            "-t${Session}:%999999", '-t', '-at', '--target', '--target=', '-t=', '-1tbad', '-_tbad', '-Ztbad')) {
            $before = State
            $reply = Tcp "kill-window $targetFlags" -Control:$control
            Check ($reply -match 'ERROR:|%error ') "server rejects explicit target spelling $targetFlags (control=$control)"
            Check ((State) -eq $before) "target spelling cannot kill the active window (control=$control)"
        }
        foreach ($command in @('kill-window', 'kill-pane', 'select-window', 'rename-window', 'respawn-window', 'unlink-window', 'rotate-window', 'select-layout', 'next-layout', 'previous-layout', 'set-window-option')) {
            $before = State
            $tail = if ($command -eq 'rename-window') { ' WRONG' } else { '' }
            if ($command -eq 'set-window-option') { $tail = ' @window-target-probe WRONG' }
            $reply = Tcp "$command -t${Session}:%bad$tail" -Control:$control
            Check ($reply -match 'ERROR:|%error ') "server rejects malformed pane for $command (control=$control)"
            Check ((State) -eq $before) "server preserves all state after $command failure (control=$control)"
        }
        foreach ($command in @('move-window', 'swap-window', 'link-window')) {
            foreach ($sourceFlags in @("-s${Session}:%bad", "--source=${Session}:%bad", "-ds${Session}:%bad", '-s')) {
                $before = State
                $destination = if ($sourceFlags -eq '-s') { '' } else { " --target=${Session}:9" }
                $reply = Tcp "$command $sourceFlags$destination" -Control:$control
                Check ($reply -match 'ERROR:|%error ') "server rejects source spelling $sourceFlags for $command (control=$control)"
                Check ((State) -eq $before) "$command cannot substitute an active source (control=$control)"
            }
        }
        $r = Px -Command @('new-window', '-d', '-t', $Session, '-n', 'socket-victim', 'cmd.exe /k')
        if ($r.Code -ne 0) { throw $r.Output }
        $panes = Px -Command @('list-panes', '-s', '-t', $Session, '-F', '#{window_name}|#{window_id}|#{pane_id}')
        $victim = ($panes.Output -split "`r?`n" | Where-Object { $_ -like 'socket-victim|*' }) -split '\|'
        $flags = if ($control) { "--target=${Session}:$($victim[2])" } else { "-t${Session}:$($victim[2])" }
        $reply = Tcp "kill-window $flags" -Control:$control
        Check ($reply -notmatch 'ERROR:|%error ') "server accepts pane target (control=$control)"
        $after = Windows
        Check ($after -notmatch [regex]::Escape($victim[1] + ':')) "server kills only pane's window (control=$control)"
        Check ($after -match [regex]::Escape($survivor[1] + ':survivor:1')) "server preserves active survivor (control=$control)"
    }
    Write-Host "Passed $script:Checks checks in isolated namespace $Namespace"
} finally {
    # No kill-server, even for cleanup. Only our explicitly named test session.
    $null = Px -Command @('kill-session', '-t', $Session)
    Remove-Item -LiteralPath $Config -ErrorAction SilentlyContinue
    $env:PSMUX_NO_WARM = $SavedWarm
    $DefaultAfter = @(& $Binary list-sessions -F '#{session_name}' 2>$null | Sort-Object)
    $DefaultAfter | Set-Content -LiteralPath "$SnapshotPrefix-default-after.txt"
    if (($DefaultBefore -join "`n") -ne ($DefaultAfter -join "`n")) {
        Compare-Object $DefaultBefore $DefaultAfter | Format-Table | Out-Host
        throw 'Default session list changed during isolated verification; investigate before continuing.'
    }
}
