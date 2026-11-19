# Leaves a file in the participant's directory, and says so.
Set-Content -LiteralPath (Join-Path $env:HELPER_DIR "checked") -Value "checked" -Encoding ASCII
Write-Output "checked"
