$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$Adp = "C:\adp\agent-data-plane.exe"
$Config = "C:\ProgramData\Datadog\datadog.yaml"
$ApiUrl = "https://127.0.0.1:55101/agent/dogstatsd-contexts-dump"
$Result = "C:\dogstatsd-top-result"
$CopiedDump = "C:\dogstatsd-top-copied.json.zstd"
$RequestRow = "          2`tintegration.windows.top.requests`t(2 instance, 1 env)"
$QueueRow = "          1`tintegration.windows.top.queue`t(1 queue)"

function Invoke-AdpDogStatsD([string[]] $Arguments) {
    $Output = @(& $Adp --config $Config dogstatsd @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "ADP command failed ($LASTEXITCODE): $($Arguments -join ' ')`n$($Output -join "`n")"
    }
    return $Output -join "`n"
}

function Assert-UnauthorizedRequest {
    try {
        Invoke-WebRequest -Uri $ApiUrl -Method Post -SkipCertificateCheck -TimeoutSec 10 | Out-Null
        throw "Context dump API accepted an unauthenticated request."
    } catch {
        $Response = $_.Exception.Response
        if ($null -eq $Response -or [int] $Response.StatusCode -ne 401) {
            throw
        }
    }
}

function Send-DogStatsDContexts {
    $Payload = @(
        "integration.windows.top.requests:1|c|#host:windows-a,env:prod,instance:a",
        "integration.windows.top.requests:1|c|#host:windows-b,env:prod,instance:b",
        "integration.windows.top.queue:1|c|#queue:alpha"
    ) -join "`n"
    $Bytes = [System.Text.Encoding]::UTF8.GetBytes($Payload)
    $Client = [System.Net.Sockets.UdpClient]::new()
    try {
        1..3 | ForEach-Object { [void] $Client.Send($Bytes, $Bytes.Length, "127.0.0.1", 58125) }
    } finally {
        $Client.Dispose()
    }
}

function Wait-ForOnlineReport {
    $LastOutput = ""
    foreach ($Attempt in 1..40) {
        $LastOutput = Invoke-AdpDogStatsD @("top")
        if ($LastOutput.Contains($RequestRow) -and $LastOutput.Contains($QueueRow)) {
            return $LastOutput
        }
        Start-Sleep -Milliseconds 500
    }
    throw "Retained contexts did not appear in online output:`n$LastOutput"
}

function Get-DumpPath([string] $Output) {
    $Lines = @($Output -split "`r?`n" | Where-Object { $_ -ne "" })
    if ($Lines.Count -ne 1 -or -not $Lines[0].StartsWith("Wrote ")) {
        throw "Unexpected dump-contexts output: $Output"
    }
    $Path = $Lines[0].Substring("Wrote ".Length)
    if (-not [System.IO.Path]::IsPathRooted($Path) -or [System.IO.Path]::GetFileName($Path) -ne "dogstatsd_contexts.json.zstd") {
        throw "Unexpected context dump path: $Path"
    }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Context dump was not created: $Path"
    }
    return $Path
}

function Assert-ProtectedArtifactAcl([string] $Path) {
    $Acl = Get-Acl -LiteralPath $Path
    if (-not $Acl.AreAccessRulesProtected) {
        throw "Context dump DACL permits inherited access: $Path"
    }

    $Rules = @($Acl.Access)
    $Sids = @($Rules | ForEach-Object {
        $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
    })
    $ExpectedSids = @("S-1-3-4", "S-1-5-18", "S-1-5-32-544")
    foreach ($ExpectedSid in $ExpectedSids) {
        if ($Sids -notcontains $ExpectedSid) {
            throw "Context dump DACL is missing $ExpectedSid; actual SIDs: $($Sids -join ', ')"
        }
    }
    if (@($Sids | Sort-Object -Unique).Count -ne 3) {
        throw "Context dump DACL contains unexpected trustees: $($Sids -join ', ')"
    }
    foreach ($Rule in $Rules) {
        if ($Rule.AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow) {
            throw "Context dump DACL contains a deny rule: $Rule"
        }
        if (($Rule.FileSystemRights -band [System.Security.AccessControl.FileSystemRights]::FullControl) -ne
            [System.Security.AccessControl.FileSystemRights]::FullControl) {
            throw "Context dump DACL does not grant full control to its intended trustee: $Rule"
        }
    }
}

Assert-UnauthorizedRequest
Send-DogStatsDContexts
$Online = Wait-ForOnlineReport
if (-not $Online.StartsWith("Wrote ")) {
    throw "Online top did not print the generated artifact path:`n$Online"
}

$DumpPath = Get-DumpPath (Invoke-AdpDogStatsD @("dump-contexts"))
Assert-ProtectedArtifactAcl $DumpPath
Copy-Item -LiteralPath $DumpPath -Destination $CopiedDump -Force
$Offline = Invoke-AdpDogStatsD @("top", "--path", $CopiedDump)
if (-not $Offline.Contains($RequestRow) -or -not $Offline.Contains($QueueRow)) {
    throw "Offline report is missing retained contexts:`n$Offline"
}

Set-Content -LiteralPath $Result -Value "passed" -Encoding ASCII
