# The entrypoint of panoramic's `agent-adp` participant in a Windows container. No image is built with it: panoramic
# places it in the container.
#
# It starts the Agent and ADP, writes each program's output to its log and its exit to an exit record, restarts
# nothing, and keeps the container up after both programs exit, so the harness can read what they left.

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$AgentEntrypoint = "C:\entrypoint.exe"
$Adp = "C:\adp\agent-data-plane.exe"
# agent-adp places this file when the case sets with.own_ip; keep the path in step with own_ip.rs.
$OwnIp = "C:\adp\panoramic-own-ip.ps1"

function Initialize-LogDirectories {
    # The rolling-file appender will not create parent directories on its own,
    # so make sure both the platform default and any explicit overrides exist.
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

function Write-ExitRecord([string] $Record, [long] $Code) {
    $Seconds = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000
    $Time = $Seconds.ToString("F3", [Globalization.CultureInfo]::InvariantCulture)
    $Line = '{"code":' + $Code + ',"signal":0,"time":"' + $Time + '"}' + "`n"
    [System.IO.File]::AppendAllText((Join-Path $env:PANORAMIC_EXIT_DIR $Record), $Line)
}

function Format-Argument([string] $Argument) {
    if ($Argument -match '[\s"]') { return '"' + ($Argument -replace '"', '\"') + '"' }
    return $Argument
}

# cmd's redirection appends the program's own bytes to the log as they are written, standard error merged, and
# cmd exits with the program's code.
function Start-Supervised([string] $FilePath, [string[]] $Arguments, [string] $Log) {
    $Words = @($FilePath) + $Arguments | ForEach-Object { Format-Argument $_ }
    $Line = ($Words -join ' ') + ' >> ' + (Format-Argument (Join-Path $env:PANORAMIC_LOG_DIR $Log)) + ' 2>&1'
    $Process = Start-Process -FilePath "cmd.exe" -ArgumentList "/d /s /c `"$Line`"" -WorkingDirectory "C:\" `
        -NoNewWindow -PassThru
    # Without reading the handle now, ExitCode is empty once the process has exited.
    $null = $Process.Handle
    return $Process
}

if (Test-Path $OwnIp) { . $OwnIp }
Initialize-LogDirectories
New-Item -ItemType Directory -Force "C:\ProgramData\Datadog" | Out-Null
if (-not (Test-Path "C:\ProgramData\Datadog\datadog.yaml")) {
    New-Item -ItemType File -Force "C:\ProgramData\Datadog\datadog.yaml" | Out-Null
}
if (-not (Test-Path $AgentEntrypoint)) { throw "The image has no Datadog Agent entrypoint at $AgentEntrypoint." }

# The Agent first, so ADP finds its IPC channel during bootstrap, as the image's own entrypoint does.
$Running = [ordered]@{}
$Running["agent"] = Start-Supervised $AgentEntrypoint @("datadogagent") "agent.log"
$Running["data-plane"] = Start-Supervised $Adp $args "adp.log"
while ($Running.Count -gt 0) {
    foreach ($Record in @($Running.Keys)) {
        $Process = $Running[$Record]
        if ($Process.HasExited) {
            Write-ExitRecord $Record $Process.ExitCode
            Write-Host "[*] $Record exited with code $($Process.ExitCode)."
            $Running.Remove($Record)
        }
    }
    Start-Sleep -Milliseconds 100
}
# The harness still follows the logs and reads the records, and stops the container itself.
Write-Host "[*] The Agent and ADP have exited."
while ($true) { Start-Sleep -Seconds 60 }
