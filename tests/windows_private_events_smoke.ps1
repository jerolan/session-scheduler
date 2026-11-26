# Run with PowerShell 7 on Windows against cross-compiled Windows binaries:
# pwsh -File tests/windows_private_events_smoke.ps1 -SchedulerExe <path> -FixtureExe <path>
# Compile tests/acp_fixture_windows.rs as a standalone executable with the same Windows target and linker.
[CmdletBinding(DefaultParameterSetName='Fixture')]
param(
    [Parameter(Mandatory)][string]$SchedulerExe,
    [Parameter(Mandatory, ParameterSetName='Fixture')][string]$FixtureExe,
    [Parameter(Mandatory, ParameterSetName='OpenCode')][string]$OpenCodeExe,
    [Parameter(Mandatory, ParameterSetName='OpenCode')][string]$ProxyWorkspace,
    [Parameter(Mandatory, ParameterSetName='OpenCode')][string]$ReadOnlyPrompt,
    [Parameter(Mandatory, ParameterSetName='OpenCode')][string]$ExpectedOutput
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Run this smoke test on Windows.' }
$SchedulerExe = (Resolve-Path -LiteralPath $SchedulerExe).Path
$useOpenCode = $PSCmdlet.ParameterSetName -eq 'OpenCode'
if ($useOpenCode) {
    $OpenCodeExe = (Resolve-Path -LiteralPath $OpenCodeExe).Path
    $ProxyWorkspace = (Resolve-Path -LiteralPath $ProxyWorkspace).Path
    if (-not (Test-Path -LiteralPath (Join-Path $ProxyWorkspace 'AGENTS.md') -PathType Leaf)) {
        throw 'Expected AGENTS.md in the SSH proxy workspace.'
    }
} else {
    $FixtureExe = (Resolve-Path -LiteralPath $FixtureExe).Path
}
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
function Next-Message([int]$timeout = 10) {
    $deadline = [DateTime]::UtcNow.AddSeconds($timeout)
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
$names = 'SESSION_SCHEDULER_HTTP_PORT','SESSION_SCHEDULER_BEARER_TOKEN','SESSION_SCHEDULER_ACCOUNT','SESSION_SCHEDULER_STATE_DIR','SESSION_SCHEDULER_CALLBACK_URL','SESSION_SCHEDULER_CALLBACK_IP','SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK','SESSION_SCHEDULER_WORKSPACES','SESSION_SCHEDULER_ACP_COMMAND','cOnTrOl_PlAnE_ApI_KeY','OPENAI_ADMIN_KEY','sEsSiOn_ScHeDuLeR_SMOKE_SECRET'
$inherited = @(Get-ChildItem Env: | Where-Object Name -Like 'SESSION_SCHEDULER_*' | ForEach-Object Name)
$environmentNames = @($names + $inherited + 'SESSION_SCHEDULER_OPENCODE_COMMAND' | Sort-Object -Unique)
$old = @{}
foreach ($name in $environmentNames) { $old[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$process = $null
try {
    foreach ($name in $environmentNames) { [Environment]::SetEnvironmentVariable($name, $null, 'Process') }
    $allowedWorkspace = if ($useOpenCode) { $ProxyWorkspace } else { $workspace }
    $values = @($port, 'test-token', 'test-account', $state, $callback, '127.0.0.1', '1', $allowedWorkspace, $FixtureExe, 'hidden', 'hidden', 'hidden')
    for ($i = 0; $i -lt $names.Count; $i++) { [Environment]::SetEnvironmentVariable($names[$i], "$($values[$i])", 'Process') }
    if ($useOpenCode) { $env:SESSION_SCHEDULER_OPENCODE_COMMAND = $OpenCodeExe }
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
    $catalog = Rpc 'events/list' @{}
    Assert ($catalog.result.events[0].name -eq 'turn.completed') 'Completion event is missing from discovery'
    $unauthorized = Invoke-WebRequest -Uri "http://127.0.0.1:$port/mcp" -Method Post -ContentType 'application/json' -Body '{}' -SkipHttpErrorCheck -TimeoutSec 5
    Assert ($unauthorized.StatusCode -eq 401) 'Unauthenticated access was not rejected'
    $subscription = @{name='turn.completed'; arguments=@{}; delivery=@{mode='webhook';url=$callback;secret=('whsec_' + [Convert]::ToBase64String($key))}}
    $response = Rpc 'events/subscribe' $subscription
    Assert ($null -eq $response.error -and $response.result.id) "Subscribe failed: $($response | ConvertTo-Json -Depth 10 -Compress)"
    $id = $response.result.id
    Check-PrivateAcl $state
    Check-PrivateAcl (Join-Path $state 'event-subscriptions.json')
    $verification = Next-Message; $body = $verification.body | ConvertFrom-Json
    Assert ($body.type -eq 'verification' -and $verification.subscription -eq $id) 'Verification or subscription ID mismatch'
    function Check-Turn([string]$prompt, [string]$reason, [string]$sessionId) {
        $call = if ($sessionId) {
            @{name='follow_up';arguments=@{session_id=$sessionId;prompt=$prompt}}
        } else {
            @{name='delegate';arguments=@{workspace=$workspace;prompt=$prompt}}
        }
        $response = Rpc 'tools/call' $call
        Assert ($null -eq $response.error -and -not $response.result.isError) "$($call.name) failed: $($response | ConvertTo-Json -Depth 10 -Compress)"
        $ids = $response.result.content[0].text | ConvertFrom-Json
        $message = Next-Message; $event = $message.body | ConvertFrom-Json
        Assert ($message.subscription -eq $id -and $event.name -eq 'turn.completed' -and $event.eventId -and $event.data.session_id -eq $ids.session_id -and $event.data.turn_id -eq $ids.turn_id -and $event.data.reason -eq $reason) "Completion mismatch: $($message.body)"
        return @{ids=$ids;event=$event}
    }
    function Inspect([string]$sessionId) {
        $response = Rpc 'tools/call' @{name='inspect_session';arguments=@{session_id=$sessionId}}
        Assert ($null -eq $response.error -and -not $response.result.isError) 'Session inspection failed'
        return $response.result.content[0].text | ConvertFrom-Json
    }
    if ($useOpenCode) {
        $tools = Rpc 'tools/list' @{}
        $delegate = $tools.result.tools | Where-Object name -eq 'delegate'
        Assert ('opencode' -in $delegate.inputSchema.properties.agent.enum) 'OpenCode missing from MCP tool discovery'
        $delegation = Rpc 'tools/call' @{name='delegate';arguments=@{agent='opencode';workspace=$ProxyWorkspace;prompt=$ReadOnlyPrompt}}
        Assert ($null -eq $delegation.error -and -not $delegation.result.isError) 'OpenCode delegation was rejected'
        $ids = $delegation.result.content[0].text | ConvertFrom-Json
        Assert ($ids.session_id -and $ids.turn_id) 'Delegation identifiers are missing'
        $message = Next-Message 60
        $event = $message.body | ConvertFrom-Json
        Assert ($message.subscription -eq $id -and $event.name -eq 'turn.completed' -and $event.eventId -and $event.data.session_id -eq $ids.session_id -and $event.data.turn_id -eq $ids.turn_id) 'OpenCode completion IDs did not match'
        $inspection = Rpc 'tools/call' @{name='inspect_session';arguments=@{session_id=$ids.session_id}}
        Assert ($null -eq $inspection.error -and -not $inspection.result.isError) 'OpenCode session inspection failed'
        $session = $inspection.result.content[0].text | ConvertFrom-Json
        Assert ($session.session_id -eq $ids.session_id -and $session.turn_id -eq $ids.turn_id -and $session.agent -eq 'opencode') 'OpenCode inspection IDs did not match'
        Assert $session.acp_session_id 'OpenCode did not open an ACP session'
        $reportedWorkspace = $session.workspace
        if ($reportedWorkspace.StartsWith('\\?\')) { $reportedWorkspace = $reportedWorkspace.Substring(4) }
        Assert ($reportedWorkspace -eq $ProxyWorkspace) 'OpenCode session used a different workspace'
        Assert ($session.status -eq 'completed' -and $session.stop_reason -eq 'end_turn' -and $event.data.reason -eq 'end_turn') "OpenCode did not complete: status=$($session.status) reason=$($event.data.reason)"
        $completedTool = $session.history | Where-Object { $_.type -eq 'tool_call' -and $_.status -eq 'completed' }
        Assert $completedTool 'OpenCode did not report a completed tool call'
        Assert ($session.result.Trim() -ceq $ExpectedOutput) 'OpenCode did not return the expected SSH output'
        [ordered]@{
            challenge_verified=$true; signature_verified=$true; agent=$session.agent;
            session_id=$ids.session_id; turn_id=$ids.turn_id; acp_session_id=$session.acp_session_id;
            subscription_id=$id; event_id=$event.eventId; name=$event.name; reason=$event.data.reason;
            status=$session.status; workspace=$reportedWorkspace; result=$session.result.Trim()
        } | ConvertTo-Json -Compress
        return
    }
    $null = Check-Turn 'permission' 'end_turn'
    $first = Check-Turn 'Remember blue plum' 'end_turn'
    $before = Inspect $first.ids.session_id
    $second = Check-Turn 'What did I ask you to remember?' 'end_turn' $first.ids.session_id
    $after = Inspect $second.ids.session_id
    Assert ($first.ids.session_id -eq $second.ids.session_id -and $before.acp_session_id -and $before.acp_session_id -eq $after.acp_session_id) 'Follow-up did not reuse the scheduler and ACP session IDs'
    Assert ($first.ids.turn_id -ne $second.ids.turn_id -and $first.event.eventId -ne $second.event.eventId) 'Follow-up completion was not distinguishable'
    Assert ($after.result -ceq 'blue plum') 'Follow-up did not recall the first turn'
    Assert (($after.history.text -join '|') -ceq 'Remember blue plum|Remembered.|What did I ask you to remember?|blue plum') 'Follow-up did not preserve the first turn in history'
    $null = Check-Turn 'error' 'failed'
    $recoverySubscription = Rpc 'events/subscribe' @{name='turn.recovery_required';arguments=@{};delivery=$subscription.delivery}
    $noticeSubscriptionId = $recoverySubscription.result.id
    Assert ($noticeSubscriptionId -and $noticeSubscriptionId -ne $id) 'Recovery event subscription failed'
    $verification = Next-Message
    Assert ($verification.subscription -eq $noticeSubscriptionId) 'Recovery challenge subscription ID mismatch'
    $delegation = Rpc 'tools/call' @{name='delegate';arguments=@{workspace=$workspace;prompt='disconnect with pong'}}
    Assert (-not $delegation.result.isError) 'Interrupted turn was rejected'
    $interrupted = $delegation.result.content[0].text | ConvertFrom-Json
    $message = Next-Message; $notice = $message.body | ConvertFrom-Json
    Assert ($message.subscription -eq $noticeSubscriptionId -and $notice.name -eq 'turn.recovery_required' -and $notice.data.session_id -eq $interrupted.session_id -and $notice.data.turn_id -eq $interrupted.turn_id) 'Recovery notice IDs did not match'
    $unresolved = Inspect $interrupted.session_id
    Assert ($unresolved.status -eq 'unknown' -and $null -eq $unresolved.stop_reason -and $unresolved.result -ceq 'pong') 'Last known state was lost'
    New-Item -ItemType File -Path (Join-Path $workspace 'outcome-ready') | Out-Null
    $reconnection = Rpc 'tools/call' @{name='reconnect_session';arguments=@{session_id=$interrupted.session_id}}
    Assert (-not $reconnection.result.isError) 'Explicit reconnection was rejected'
    $message = Next-Message; $completion = $message.body | ConvertFrom-Json
    Assert ($message.subscription -eq $id -and $completion.name -eq 'turn.completed' -and $completion.eventId -ne $notice.eventId -and $completion.data.session_id -eq $interrupted.session_id -and $completion.data.turn_id -eq $interrupted.turn_id -and $completion.data.reason -eq 'end_turn') 'Recovered completion IDs did not match'
    $restored = Inspect $interrupted.session_id
    Assert ($restored.status -eq 'completed' -and $restored.acp_session_id -eq $unresolved.acp_session_id -and $restored.result -ceq 'pong completo' -and ($restored.history.text -join '|') -ceq 'disconnect with pong|pong| completo') 'Recovery did not reconcile the complete result and history'
    Remove-Item -LiteralPath (Join-Path $workspace 'outcome-ready'),(Join-Path $workspace 'interrupted.txt')
    $delegation = Rpc 'tools/call' @{name='delegate';arguments=@{workspace=$workspace;prompt='disconnect with pong'}}
    Assert (-not $delegation.result.isError) 'Second interrupted turn was rejected'
    $unknown = $delegation.result.content[0].text | ConvertFrom-Json
    $message = Next-Message; $notice = $message.body | ConvertFrom-Json
    Assert ($message.subscription -eq $noticeSubscriptionId -and $notice.name -eq 'turn.recovery_required' -and $notice.data.session_id -eq $unknown.session_id -and $notice.data.turn_id -eq $unknown.turn_id) 'Unknown turn notice IDs did not match'
    $beforeFollowUp = Inspect $unknown.session_id
    Assert ($beforeFollowUp.status -eq 'unknown' -and $null -eq $beforeFollowUp.stop_reason) 'Unconfirmed original turn acquired a terminal outcome'
    $followUp = Check-Turn 'What was the interrupted response?' 'end_turn' $unknown.session_id
    $afterFollowUp = Inspect $unknown.session_id
    Assert ($followUp.ids.turn_id -ne $unknown.turn_id -and $afterFollowUp.acp_session_id -eq $beforeFollowUp.acp_session_id -and $afterFollowUp.result -ceq 'pong') 'Explicit follow-up did not reuse loaded context with a new turn'
    Assert ($unknown.turn_id -in $afterFollowUp.unresolved_turn_ids) 'The original indeterminate turn ID was lost'
    $tools = Rpc 'tools/list' @{}
    Assert ('revoke_workspace' -in $tools.result.tools.name) 'Revocation missing from MCP discovery'
    $revocation = Rpc 'tools/call' @{name='revoke_workspace';arguments=@{workspace=$workspace}}
    $revoked = $revocation.result.content[0].text | ConvertFrom-Json
    Assert (-not $revocation.result.isError -and $revoked.status -eq 'revoked' -and $revoked.workspace -ceq $after.workspace) 'Revocation did not confirm the exact workspace'
    Check-PrivateAcl (Join-Path $state 'trusted-workspaces.json')
    $blocked = Rpc 'tools/call' @{name='follow_up';arguments=@{session_id=$first.ids.session_id;prompt='blocked'}}
    Assert ($blocked.result.isError -and ($blocked.result.content[0].text | ConvertFrom-Json).status -eq 'workspace_not_trusted') 'Revocation did not block follow-up'
    $blocked = Rpc 'tools/call' @{name='reconnect_session';arguments=@{session_id=$unknown.session_id;turn_id=$unknown.turn_id}}
    Assert ($blocked.result.isError -and ($blocked.result.content[0].text | ConvertFrom-Json).status -eq 'workspace_not_trusted') 'Revocation did not block reconnection'
    $afterRevoke = Inspect $first.ids.session_id
    Assert (($afterRevoke.history.text -join '|') -ceq ($after.history.text -join '|')) 'Revocation changed readable history'

    $null = Rpc 'events/unsubscribe' @{name='turn.recovery_required';arguments=@{};delivery=@{mode='webhook';url=$callback}}
    Stop-Scheduler
    Start-Scheduler
    $delegation = Rpc 'tools/call' @{name='delegate';arguments=@{workspace=$workspace;prompt='error'}}
    $pending = $delegation.result.content[0].text | ConvertFrom-Json
    Assert (-not $delegation.result.isError -and $pending.status -eq 'approval_required' -and $pending.workspace -ceq $revoked.workspace) 'Revoked startup workspace regained trust after restart'
    $sessions = Rpc 'tools/call' @{name='list_sessions';arguments=@{}}
    Assert ($sessions.result.content[0].text -ceq '[]') 'Unapproved delegation created a session'
    $approval = @{name='approve_workspace';arguments=@{attempt_id=$pending.attempt_id}}
    $approved = Rpc 'tools/call' $approval
    $retry = Rpc 'tools/call' $approval
    $approvedIds = $approved.result.content[0].text | ConvertFrom-Json
    Assert (-not $approved.result.isError -and $approvedIds.session_id -and $approvedIds.turn_id -and -not $retry.result.isError -and $retry.result.content[0].text -ceq $approved.result.content[0].text) 'Explicit reapproval was not idempotent'
    # The original subscription must deliver before any attempt to subscribe again.
    $message = Next-Message; $event = $message.body | ConvertFrom-Json
    Assert ($message.subscription -eq $id -and $event.name -eq 'turn.completed' -and $event.data.session_id -eq $approvedIds.session_id -and $event.data.turn_id -eq $approvedIds.turn_id -and $event.data.reason -eq 'failed') 'Reapproved turn completion was not delivered to the original subscription'
    $sessions = Rpc 'tools/call' @{name='list_sessions';arguments=@{}}
    Assert (@(($sessions.result.content[0].text | ConvertFrom-Json)).Count -eq 1) 'Reapproval created duplicate sessions'
    $null = Check-Turn 'error' 'failed'
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
    Write-Host 'PASS: explicit follow-up kept ACP session and context with distinct signed completions'
    Write-Host 'PASS: recovered history and result matched the original turn; explicit follow-up reused an unconfirmed session'
    Write-Host 'PASS: persisted subscription, atomic replacement, protected ACLs and denied ACP approval'
    Write-Host 'PASS: exact-workspace revocation, readable history, blocked follow-up and reconnect, persisted revocation and idempotent reapproval'
    Write-Host 'PASS: inherited ACL, junction and junction ancestor were rejected'
} finally {
    Stop-Scheduler
    New-Item -ItemType File -Path $stopReceiver -Force | Out-Null
    $null = Wait-Job $receiver -Timeout 8
    Remove-Job $receiver -Force -ErrorAction SilentlyContinue
    foreach ($name in $environmentNames) { [Environment]::SetEnvironmentVariable($name, $old[$name], 'Process') }
    if (Test-Path -LiteralPath $workspace) {
        $directory = Get-Item -LiteralPath $workspace -Force
        $expectedParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
        if ($directory.Parent.FullName -eq $expectedParent -and $directory.Name -match '^scheduler-smoke-[0-9a-f-]{36}$' -and -not ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            Remove-Item -LiteralPath $directory.FullName -Recurse -Force
        }
    }
}
