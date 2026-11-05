function Get-ContainerIPv4Address {
    # Use hostname resolution to obtain an IPv4 address on the Agent LTSC image.
    $Addresses = [System.Net.Dns]::GetHostAddresses([System.Net.Dns]::GetHostName())
    foreach ($Addr in $Addresses) {
        if ($Addr.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork) {
            $Text = $Addr.IPAddressToString
            if ($Text -ne "127.0.0.1" -and $Text -ne "0.0.0.0") {
                return $Text
            }
        }
    }
    throw "panoramic: could not determine the participant IP address"
}

$Ip = Get-ContainerIPv4Address
$Placeholder = '@IP_REFERENCE@'
foreach ($Item in Get-ChildItem Env:) {
    if ($Item.Value.Contains($Placeholder)) {
        Set-Item -Path "Env:\$($Item.Name)" -Value $Item.Value.Replace($Placeholder, $Ip)
    }
}
