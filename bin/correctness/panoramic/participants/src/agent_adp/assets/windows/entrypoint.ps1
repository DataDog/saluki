$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Initialize-LogDirectories {
    # ADP's rolling-file appender requires the parent directory to exist.
    $Paths = @("C:\ProgramData\Datadog\logs")
    foreach ($Var in @("DD_DATA_PLANE_LOG_FILE", "DD_LOG_FILE")) {
        $Value = [Environment]::GetEnvironmentVariable($Var)
        if ($Value) {
            $Paths += [System.IO.Path]::GetDirectoryName($Value)
        }
    }
    foreach ($Path in $Paths) {
        if ($Path) {
            New-Item -ItemType Directory -Force -Path $Path | Out-Null
        }
    }
}

# Install the same record writer in the foreground and the Agent waiter's runspace.
$InitializeExitRecords = {
    function Write-ExitRecord {
        param([string]$Name, [int]$Code)
        $UnsignedCode = [BitConverter]::ToUInt32([BitConverter]::GetBytes($Code), 0)
        $Now = [DateTimeOffset]::UtcNow
        $Record = "{0} 0 {1}.{2:D7}`n" -f $UnsignedCode, $Now.ToUnixTimeSeconds(), ($Now.Ticks % [TimeSpan]::TicksPerSecond)
        [System.IO.File]::WriteAllText("$env:PANORAMIC_EXIT_DIR\$Name", $Record)
    }
}
. $InitializeExitRecords

if (Test-Path "C:\adp\participant-late.ps1") { . "C:\adp\participant-late.ps1" }
Initialize-LogDirectories
$Agent = $null
$AgentWait = $null
if (Test-Path "C:\entrypoint.exe") {
    New-Item -ItemType Directory -Force "C:\ProgramData\Datadog" | Out-Null
    if (-not (Test-Path "C:\ProgramData\Datadog\datadog.yaml")) {
        New-Item -ItemType File -Force "C:\ProgramData\Datadog\datadog.yaml" | Out-Null
    }

    # cmd redirects the Agent's bytes directly into the log.
    $Agent = Start-Process -FilePath "cmd.exe" `
        -ArgumentList @("/d", "/s", "/c", '""C:\entrypoint.exe" datadogagent > "%PANORAMIC_LOG_DIR%\agent.log" 2>&1"') `
        -WorkingDirectory "C:\" -NoNewWindow -PassThru
    $AgentWait = Start-ThreadJob -InitializationScript $InitializeExitRecords -ArgumentList $Agent -ScriptBlock {
        param($AgentProcess)
        $ErrorActionPreference = "Stop"
        $AgentProcess.WaitForExit()
        Write-ExitRecord "agent" $AgentProcess.ExitCode
    }
}

try {
    & "C:\adp\agent-data-plane.exe" @args
    $ExitCode = $LASTEXITCODE
    Write-ExitRecord "adp" $ExitCode
} finally {
    if ($Agent -and -not $Agent.HasExited) {
        try {
            # The tracked process is cmd; stop the Agent and its children too.
            $Agent.Kill($true)
        } catch {
            if (-not $Agent.HasExited) { throw }
        }
    }
    if ($AgentWait) {
        $AgentWait | Wait-Job | Receive-Job -ErrorAction Stop
        Remove-Job $AgentWait
    }
}

exit $ExitCode
