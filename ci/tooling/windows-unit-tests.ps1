$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)]
        [string]$FilePath,

        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]]$Arguments
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: ${FilePath} $($Arguments -join ' ')"
    }
}

function Add-PathEntry {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    if ((Test-Path $Path) -and (-not ($env:Path -split ';' | Where-Object { $_ -eq $Path }))) {
        $env:Path = "${Path};${env:Path}"
    }
}

function Ensure-Protoc {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Version,

        [Parameter(Mandatory = $true)]
        [string]$InstallRoot
    )

    $ProtocBin = Join-Path $InstallRoot "bin\protoc.exe"
    if (-not (Test-Path $ProtocBin)) {
        Write-Host "[*] protoc not found; installing protoc ${Version}..."
        $Archive = Join-Path $env:TEMP "protoc-${Version}-win64.zip"
        $ExtractRoot = Join-Path $env:TEMP "protoc-${Version}-win64"
        Remove-Item -Recurse -Force $ExtractRoot -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force $InstallRoot | Out-Null
        Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/protocolbuffers/protobuf/releases/download/v${Version}/protoc-${Version}-win64.zip" -OutFile $Archive
        Expand-Archive -Path $Archive -DestinationPath $ExtractRoot -Force
        Copy-Item -Recurse -Force (Join-Path $ExtractRoot "*") $InstallRoot
    }

    $env:PROTOC = $ProtocBin
    Add-PathEntry (Join-Path $InstallRoot "bin")
    Invoke-Native $ProtocBin --version
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

$OriginalCargoHome = $env:CARGO_HOME
if (-not $env:WINDOWS_CI_CARGO_HOME) {
    $env:WINDOWS_CI_CARGO_HOME = Join-Path $RepoRoot ".ci-cache\cargo"
}
if (-not $env:RUSTUP_HOME) {
    $env:RUSTUP_HOME = Join-Path $RepoRoot ".ci-cache\rustup"
}
if (-not $env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR = Join-Path $RepoRoot "target\windows-ci"
}
if (-not $env:CARGO_NEXTEST_VERSION) {
    $env:CARGO_NEXTEST_VERSION = "0.9.99"
}
if (-not $env:PROTOC_VERSION) {
    $env:PROTOC_VERSION = "29.3"
}
if (-not $env:WINDOWS_CI_PROTOC_HOME) {
    $env:WINDOWS_CI_PROTOC_HOME = Join-Path $RepoRoot ".ci-cache\protoc\$env:PROTOC_VERSION"
}
if (-not $env:WINDOWS_CI_PACKAGES) {
    $env:WINDOWS_CI_PACKAGES = "datadog-agent-commons,process-memory,saluki-error,ddsketch,prometheus-exposition,saluki-config"
}

New-Item -ItemType Directory -Force $env:WINDOWS_CI_CARGO_HOME | Out-Null
New-Item -ItemType Directory -Force $env:WINDOWS_CI_PROTOC_HOME | Out-Null
New-Item -ItemType Directory -Force $env:RUSTUP_HOME | Out-Null
New-Item -ItemType Directory -Force $env:CARGO_TARGET_DIR | Out-Null
if ($OriginalCargoHome) {
    Add-PathEntry (Join-Path $OriginalCargoHome "bin")
}
Add-PathEntry (Join-Path $env:USERPROFILE ".cargo\bin")

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "[*] Rust toolchain not found; installing rustup..."
    $RustupInit = Join-Path $env:TEMP "rustup-init.exe"
    Invoke-WebRequest -UseBasicParsing -Uri "https://win.rustup.rs/x86_64" -OutFile $RustupInit
    Invoke-Native $RustupInit -y --profile minimal --default-toolchain none
    if ($env:CARGO_HOME) {
        Add-PathEntry (Join-Path $env:CARGO_HOME "bin")
    }
    Add-PathEntry (Join-Path $env:USERPROFILE ".cargo\bin")
}

$ToolchainMatch = Select-String -Path "rust-toolchain.toml" -Pattern 'channel\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $ToolchainMatch) {
    throw "Unable to determine Rust toolchain from rust-toolchain.toml"
}
$Toolchain = $ToolchainMatch.Matches[0].Groups[1].Value

Write-Host "[*] Ensuring Rust toolchain ${Toolchain} is installed..."
Invoke-Native rustup toolchain install --profile minimal $Toolchain
Invoke-Native rustup default $Toolchain
Invoke-Native rustup show
Invoke-Native cargo --version

$env:CARGO_HOME = $env:WINDOWS_CI_CARGO_HOME
Add-PathEntry (Join-Path $env:CARGO_HOME "bin")
Ensure-Protoc -Version $env:PROTOC_VERSION -InstallRoot $env:WINDOWS_CI_PROTOC_HOME

if (-not (Get-Command cargo-nextest -ErrorAction SilentlyContinue)) {
    Write-Host "[*] cargo-nextest not found; installing cargo-nextest@$env:CARGO_NEXTEST_VERSION..."
    Invoke-Native cargo install cargo-nextest --version $env:CARGO_NEXTEST_VERSION --locked
}
Invoke-Native cargo nextest --version

$PackageArgs = @()
$Packages = $env:WINDOWS_CI_PACKAGES -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ }
foreach ($Package in $Packages) {
    $PackageArgs += @("-p", $Package)
}

Write-Host "[*] Running Windows unit tests for packages: $($Packages -join ', ')"
$NextestArgs = @("nextest", "run") + $PackageArgs + @("--lib", "--bins", "--no-fail-fast", "-E", "not test(/property_test_*/)")
Invoke-Native -FilePath cargo -Arguments $NextestArgs
