$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Get-ContainerIPv4Address {
    # The Datadog Agent LTSC image does not ship the NetTCPIP module, so we
    # fall back to .NET DNS resolution against the container hostname.
    $Addresses = [System.Net.Dns]::GetHostAddresses([System.Net.Dns]::GetHostName())
    foreach ($Addr in $Addresses) {
        if ($Addr.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork) {
            $Text = $Addr.IPAddressToString
            if ($Text -ne "127.0.0.1" -and $Text -ne "0.0.0.0") {
                return $Text
            }
        }
    }

    throw "Unable to determine container IPv4 address."
}

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
    $Agent = Start-Process -FilePath "C:\entrypoint.exe" `
        -ArgumentList @("datadogagent") `
        -WorkingDirectory "C:\" `
        -NoNewWindow `
        -PassThru

    return $Agent
}

Resolve-PanoramicDynamicEnvironment
Initialize-LogDirectories
$Agent = Start-CoreAgent

Write-Host "[*] ADP runtime directory:"
Get-ChildItem "C:\adp" | ForEach-Object { Write-Host "  $($_.Name)" }

& "C:\adp\agent-data-plane.exe" @args
$ExitCode = $LASTEXITCODE

if ($Agent -and -not $Agent.HasExited) {
    Stop-Process -Id $Agent.Id -Force -ErrorAction SilentlyContinue
}

exit $ExitCode
