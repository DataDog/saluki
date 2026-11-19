$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

if (-not $env:CARGO_NEXTEST_VERSION) {
    $env:CARGO_NEXTEST_VERSION = "0.9.152"
}

Initialize-RustEnvironment -RepoRoot $RepoRoot

# The Cargo home is cached between jobs, so reinstall cargo-nextest when the cached version doesn't match the pinned one.
$InstalledNextestVersion = $null
if (Get-Command cargo-nextest -ErrorAction SilentlyContinue) {
    $InstalledNextestVersion = ((& cargo nextest --version | Select-Object -First 1) -split " ")[1]
}
if ($InstalledNextestVersion -ne $env:CARGO_NEXTEST_VERSION) {
    Write-Host "[*] Installing cargo-nextest@$env:CARGO_NEXTEST_VERSION (found: $InstalledNextestVersion)..."
    Invoke-Native cargo install cargo-nextest --version $env:CARGO_NEXTEST_VERSION --locked --force
}
Invoke-Native cargo nextest --version

Write-Host "[*] Running Windows unit tests for the full default workspace scope."
$NextestArgs = @("nextest", "run", "--lib", "--bins", "--no-fail-fast", "-E", "not test(/property_test_*/)")
Invoke-Native -FilePath cargo -Arguments $NextestArgs
