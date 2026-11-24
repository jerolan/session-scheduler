# Run with PowerShell 7 on Windows against cross-compiled Windows binaries:
# pwsh -File tests/windows_private_events_smoke.ps1 -SchedulerExe <path> -FixtureExe <path>
# Fixture build: rustc --target x86_64-pc-windows-gnu tests/acp_fixture_windows.rs -o acp-fixture.exe
param([Parameter(Mandatory)][string]$SchedulerExe, [Parameter(Mandatory)][string]$FixtureExe)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Run this smoke test on Windows.' }
$SchedulerExe = (Resolve-Path -LiteralPath $SchedulerExe).Path
$FixtureExe = (Resolve-Path -LiteralPath $FixtureExe).Path
function Assert($condition, [string]$message) { if (-not $condition) { throw $message } }
function Free-Port { $l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $l.Start(); try { return $l.LocalEndpoint.Port } finally { $l.Stop() } }
function Rpc([string]$method, $params) {
    $body = @{jsonrpc='2.0'; id=1; method=$method; params=$params} | ConvertTo-Json -Depth 20 -Compress
    Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$port/mcp" -Headers @{Authorization='Bearer test-token'; Accept='application/json, text/event-stream'} -ContentType 'application/json' -Body ([Text.Encoding]::UTF8.GetBytes($body)) -TimeoutSec 10
}
function Check-PrivateAcl([string]$path) {
    $acl = Get-Acl -LiteralPath $path
    Assert $acl.AreAccessRulesProtected "ACL inherits permissions: $path"
    $owner = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -eq 'Allow') { Assert ($rule.IdentityReference.Value -eq $owner) "Non-owner allow ACE on $path" }
    }
    $allows = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | Where-Object AccessControlType -eq 'Allow')
    Assert ($allows.Count -gt 0) "Missing owner allow ACE on $path"
}
function Start-Scheduler {
    $script:process = Start-Process -FilePath $SchedulerExe -PassThru -WindowStyle Hidden -RedirectStandardError $stderrLog
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        if ($process.HasExited) { throw "Scheduler exited during startup: $($process.ExitCode) $(Get-Content -LiteralPath $stderrLog -Raw)" }
        try { $c = [Net.Sockets.TcpClient]::new(); $c.Connect('127.0.0.1', $port); $c.Dispose(); return } catch { Start-Sleep -Milliseconds 100 }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Scheduler did not start'
}
function Stop-Scheduler {
    if ($process) {
        if (-not $process.HasExited) { $process.Kill($true); $process.WaitForExit() }
        $process.Dispose()
    }
    $script:process = $null
}
function Next-Message {
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $messages = @(Receive-Job $receiver)
        if ($messages.Count -gt 0) {
            $m = $messages[0]; if ($m.error) { throw $m.error }
            Assert ($m.signature -ceq $m.expected) 'Invalid webhook signature'
            return $m
        }
        if ($receiver.State -ne 'Running') { throw "Receiver stopped: $($receiver.State)" }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Timed out waiting for webhook'
}
$workspace = Join-Path ([IO.Path]::GetTempPath()) ("scheduler-smoke-" + [guid]::NewGuid())
$state = Join-Path $workspace 'state'
$stderrLog = Join-Path $workspace 'scheduler.stderr'
$stopReceiver = Join-Path $workspace 'stop-receiver'
New-Item -ItemType Directory -Path $workspace | Out-Null
$port = Free-Port; $receiverPort = Free-Port
$key = [Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
$receiver = Start-Job -ArgumentList $receiverPort,([Convert]::ToBase64String($key)),$stopReceiver -ScriptBlock {
    param($port, $key64, $stopFile)
    $ErrorActionPreference = 'Stop'
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $port)
    $listener.Start()
    Write-Output @{ready=$true}
    try {
        while (-not (Test-Path -LiteralPath $stopFile)) {
            if (-not $listener.Pending()) { Start-Sleep -Milliseconds 20; continue }
            $client = $listener.AcceptTcpClient()
            try {
                $client.ReceiveTimeout = 5000; $stream = $client.GetStream()
                $head = [Collections.Generic.List[byte]]::new()
                while (($b = $stream.ReadByte()) -ge 0) {
                    $head.Add([byte]$b); $n = $head.Count
                    if ($n -gt 65536) { throw 'Oversized headers' }
                    if ($n -ge 4 -and $head[$n-4] -eq 13 -and $head[$n-3] -eq 10 -and $head[$n-2] -eq 13 -and $head[$n-1] -eq 10) { break }
                }
                $headers = @{}
                foreach ($line in ([Text.Encoding]::ASCII.GetString($head.ToArray()) -split "`r`n")) { $p = $line.IndexOf(':'); if ($p -gt 0) { $headers[$line.Substring(0,$p)] = $line.Substring($p+1).Trim() } }
                $bytes = [byte[]]::new([int]$headers['Content-Length']); $offset = 0
                while ($offset -lt $bytes.Length) { $n = $stream.Read($bytes, $offset, $bytes.Length-$offset); if ($n -eq 0) { throw 'Short request body' }; $offset += $n }
                $prefix = [Text.Encoding]::UTF8.GetBytes($headers['webhook-id'] + '.' + $headers['webhook-timestamp'] + '.')
                $signed = [byte[]]::new($prefix.Length + $bytes.Length); [Array]::Copy($prefix,0,$signed,0,$prefix.Length); [Array]::Copy($bytes,0,$signed,$prefix.Length,$bytes.Length)
                $expected = 'v1,' + [Convert]::ToBase64String([Security.Cryptography.HMACSHA256]::HashData([Convert]::FromBase64String($key64), $signed))
                $raw = [Text.Encoding]::UTF8.GetString($bytes); $body = $raw | ConvertFrom-Json
                $response = [Text.Encoding]::UTF8.GetBytes((@{challenge=$body.challenge} | ConvertTo-Json -Compress))
                $responseHead = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nContent-Length: $($response.Length)`r`nConnection: close`r`n`r`n")
                $stream.Write($responseHead); $stream.Write($response)
                Write-Output @{body=$raw;signature=$headers['webhook-signature'];expected=$expected;subscription=$headers['X-MCP-Subscription-Id']}
            } catch { Write-Output @{error=$_.ToString()} } finally { $client.Dispose() }
        }
    } finally { $listener.Stop() }
}
$callback = "http://127.0.0.1:$receiverPort/callback"
$names = 'SESSION_SCHEDULER_HTTP_PORT','SESSION_SCHEDULER_BEARER_TOKEN','SESSION_SCHEDULER_ACCOUNT','SESSION_SCHEDULER_STATE_DIR','SESSION_SCHEDULER_CALLBACK_URL','SESSION_SCHEDULER_CALLBACK_IP','SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK','SESSION_SCHEDULER_WORKSPACES','SESSION_SCHEDULER_ACP_COMMAND','CONTROL_PLANE_API_KEY','OPENAI_ADMIN_KEY'
$old = @{}; foreach ($name in $names) { $old[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$process = $null
try {
    $values = @($port, 'test-token', 'test-account', $state, $callback, '127.0.0.1', '1', $workspace, $FixtureExe, 'hidden', 'hidden')
    for ($i = 0; $i -lt $names.Count; $i++) { [Environment]::SetEnvironmentVariable($names[$i], "$($values[$i])", 'Process') }
    $ready = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        foreach ($message in @(Receive-Job $receiver)) { if ($message.ready) { $ready = $true } }
        if (-not $ready) { Start-Sleep -Milliseconds 50 }
    } until ($ready -or [DateTime]::UtcNow -ge $deadline -or $receiver.State -ne 'Running')
    Assert $ready 'Local receiver did not start'
    Start-Scheduler
    $initial = Rpc 'initialize' @{protocolVersion='2025-03-26'; capabilities=@{}; clientInfo=@{name='windows-smoke';version='1'}}
    Assert ($null -ne $initial.result.capabilities.events) 'Events capability missing'
    $subscription = @{name='turn.completed'; arguments=@{}; delivery=@{mode='webhook';url=$callback;secret=('whsec_' + [Convert]::ToBase64String($key))}}
    $response = Rpc 'events/subscribe' $subscription
    Assert ($null -eq $response.error -and $response.result.id) "Subscribe failed: $($response | ConvertTo-Json -Depth 10 -Compress)"
    $id = $response.result.id
    Check-PrivateAcl $state
    Check-PrivateAcl (Join-Path $state 'event-subscriptions.json')
    $verification = Next-Message; $body = $verification.body | ConvertFrom-Json
    Assert ($body.type -eq 'verification' -and $verification.subscription -eq $id) 'Verification or subscription ID mismatch'
    function Check-Turn([string]$prompt, [string]$reason) {
        $response = Rpc 'tools/call' @{name='delegate';arguments=@{workspace=$workspace;prompt=$prompt}}
        Assert ($null -eq $response.error) "Delegate failed: $($response | ConvertTo-Json -Depth 10 -Compress)"
        $ids = $response.result.content[0].text | ConvertFrom-Json
        $message = Next-Message; $event = $message.body | ConvertFrom-Json
        Assert ($message.subscription -eq $id -and $event.name -eq 'turn.completed' -and $event.eventId -and $event.data.session_id -eq $ids.session_id -and $event.data.turn_id -eq $ids.turn_id -and $event.data.reason -eq $reason) "Completion mismatch: $($message.body)"
    }
    Check-Turn 'success' 'end_turn'
    Check-Turn 'error' 'failed'
    Stop-Scheduler
    Start-Scheduler
    $again = Rpc 'events/subscribe' $subscription
    Assert ($again.result.id -eq $id) 'Subscription did not persist across restart'
    Check-Turn 'error' 'failed'
    $unsubscribed = Rpc 'events/unsubscribe' @{name='turn.completed';arguments=@{};delivery=@{mode='webhook';url=$callback}}
    Assert ($null -eq $unsubscribed.error) 'Unsubscribe did not atomically replace subscriptions'
    Check-PrivateAcl (Join-Path $state 'event-subscriptions.json')
    Stop-Scheduler
    $bad = Join-Path $workspace 'broad'
    New-Item -ItemType Directory -Path $bad | Out-Null
    Assert (-not (Get-Acl -LiteralPath $bad).AreAccessRulesProtected) 'Expected a directory with inherited ACLs'
    function Assert-RejectedState([string]$path, [string]$label) {
        [Environment]::SetEnvironmentVariable('SESSION_SCHEDULER_STATE_DIR', $path, 'Process')
        $script:process = Start-Process -FilePath $SchedulerExe -PassThru -WindowStyle Hidden -RedirectStandardError $stderrLog
        Assert ($process.WaitForExit(10000) -and $process.ExitCode -eq 1) "$label state was not rejected"
        Assert ((Get-Content -LiteralPath $stderrLog -Raw) -like '*owner-only protected ACL*') "$label did not fail the private-state check"
        Stop-Scheduler
    }
    Assert-RejectedState $bad 'Inherited ACL'
    Assert (@(Get-ChildItem -LiteralPath $bad -Force).Count -eq 0) 'Rejected state directory was modified'
    $junction = Join-Path $workspace 'state-link'
    New-Item -ItemType Junction -Path $junction -Target $state | Out-Null
    try {
        Assert-RejectedState $junction 'Reparse-point'
        Assert-RejectedState (Join-Path $junction 'descendant') 'Reparse-point ancestor'
        Assert (-not (Test-Path -LiteralPath (Join-Path $state 'descendant'))) 'State was created through a reparse point'
    } finally {
        Remove-Item -LiteralPath $junction -Force
    }
    Write-Host 'PASS: Windows authenticated MCP and signed verification'
    Write-Host 'PASS: signed success and confirmed-failure events matched session and turn IDs'
    Write-Host 'PASS: persisted subscription, atomic replacement, private ACLs and read-only fixture'
    Write-Host 'PASS: inherited ACL, junction and junction ancestor were rejected'
} finally {
    Stop-Scheduler
    New-Item -ItemType File -Path $stopReceiver -Force | Out-Null
    $null = Wait-Job $receiver -Timeout 8
    Remove-Job $receiver -Force -ErrorAction SilentlyContinue
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $old[$name], 'Process') }
    if (Test-Path -LiteralPath $workspace) {
        $directory = Get-Item -LiteralPath $workspace -Force
        $expectedParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
        if ($directory.Parent.FullName -eq $expectedParent -and $directory.Name -match '^scheduler-smoke-[0-9a-f-]{36}$' -and -not ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            Remove-Item -LiteralPath $directory.FullName -Recurse -Force
        }
    }
}
