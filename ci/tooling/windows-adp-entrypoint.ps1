$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Get-ContainerIPv4Address {
    $Address = Get-NetIPAddress -AddressFamily IPv4 |
        Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -ne "0.0.0.0" } |
        Select-Object -First 1 -ExpandProperty IPAddress

    if (-not $Address) {
        throw "Unable to determine container IPv4 address."
    }

    return $Address
}

function Resolve-PanoramicDynamicEnvironment {
    $DynamicValues = @{}

    if ($env:PANORAMIC_DYNAMIC_CONTAINER_IP) {
        $DynamicValues["PANORAMIC_DYNAMIC_CONTAINER_IP"] = Get-ContainerIPv4Address
    }

    if ($env:PANORAMIC_DYNAMIC_CUSTOM_HOSTNAME) {
        $Hostname = "foo.local"
        $ContainerIp = if ($DynamicValues.ContainsKey("PANORAMIC_DYNAMIC_CONTAINER_IP")) {
            $DynamicValues["PANORAMIC_DYNAMIC_CONTAINER_IP"]
        } else {
            Get-ContainerIPv4Address
        }
        Add-Content -Path "C:\Windows\System32\drivers\etc\hosts" -Value "${ContainerIp} ${Hostname}"
        $DynamicValues["PANORAMIC_DYNAMIC_CUSTOM_HOSTNAME"] = $Hostname
    }

    foreach ($Pair in $DynamicValues.GetEnumerator()) {
        Set-Item -Path "Env:\$($Pair.Key)" -Value $Pair.Value
    }

    foreach ($Item in Get-ChildItem Env:) {
        if ($Item.Name -like "DD_*" -or $Item.Name -like "PANORAMIC_DYNAMIC_*") {
            $Value = $Item.Value
            foreach ($Pair in $DynamicValues.GetEnumerator()) {
                $Value = $Value.Replace("{{$($Pair.Key)}}", $Pair.Value)
            }
            if ($Value -ne $Item.Value) {
                Set-Item -Path "Env:\$($Item.Name)" -Value $Value
            }
        }
    }
}

function Start-CoreAgent {
    if (-not (Test-Path "C:\entrypoint.exe")) {
        Write-Host "[*] Datadog Agent container entrypoint not found; running ADP without Core Agent."
        return $null
    }

    New-Item -ItemType Directory -Force "C:\ProgramData\Datadog" | Out-Null
    if (-not (Test-Path "C:\ProgramData\Datadog\datadog.yaml")) {
        New-Item -ItemType File -Force "C:\ProgramData\Datadog\datadog.yaml" | Out-Null
    }

    Write-Host "[*] Starting Datadog Agent container entrypoint..."
    $AgentStdoutLog = "C:\ProgramData\Datadog\saluki-core-agent.stdout.log"
    $AgentStderrLog = "C:\ProgramData\Datadog\saluki-core-agent.stderr.log"
    $Agent = Start-Process -FilePath "C:\entrypoint.exe" `
        -ArgumentList @("datadogagent") `
        -RedirectStandardOutput $AgentStdoutLog `
        -RedirectStandardError $AgentStderrLog `
        -PassThru

    return $Agent
}

function Wait-ForCoreAgentIpc {
    $Deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $Deadline) {
        if ((Test-Path "C:\ProgramData\Datadog\auth_token") -and (Test-Path "C:\ProgramData\Datadog\ipc_cert.pem")) {
            Write-Host "[*] Core Agent IPC credentials are present."
            return
        }
        Start-Sleep -Seconds 1
    }

    foreach ($Path in @("C:\ProgramData\Datadog\saluki-core-agent.stdout.log", "C:\ProgramData\Datadog\saluki-core-agent.stderr.log")) {
        if (Test-Path $Path) {
            Write-Host "[*] Core Agent log: ${Path}"
            Get-Content $Path
        }
    }
    throw "Timed out waiting for Core Agent IPC credentials."
}

Resolve-PanoramicDynamicEnvironment
$Agent = Start-CoreAgent
if ($Agent) {
    Wait-ForCoreAgentIpc
}

& "C:\adp\agent-data-plane.exe" @args
$ExitCode = $LASTEXITCODE

if ($Agent -and -not $Agent.HasExited) {
    Stop-Process -Id $Agent.Id -Force -ErrorAction SilentlyContinue
}

exit $ExitCode
