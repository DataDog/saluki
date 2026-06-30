$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

if (-not $env:CARGO_NEXTEST_VERSION) {
    $env:CARGO_NEXTEST_VERSION = "0.9.99"
}

Initialize-RustEnvironment -RepoRoot $RepoRoot

if (-not (Get-Command cargo-nextest -ErrorAction SilentlyContinue)) {
    Write-Host "[*] cargo-nextest not found; installing cargo-nextest@$env:CARGO_NEXTEST_VERSION..."
    Invoke-Native cargo install cargo-nextest --version $env:CARGO_NEXTEST_VERSION --locked
}
Invoke-Native cargo nextest --version

Write-Host "[*] Running Windows unit tests for the full default workspace scope."
$NextestArgs = @("nextest", "run", "--lib", "--bins", "--no-fail-fast", "-E", "not test(/property_test_*/)")
Invoke-Native -FilePath cargo -Arguments $NextestArgs
