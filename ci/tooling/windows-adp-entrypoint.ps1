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

& "C:\adp\agent-data-plane.exe" @args
exit $LASTEXITCODE
