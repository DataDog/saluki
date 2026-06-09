# Shared PowerShell helpers used by every Windows CI script that needs to drive cargo:
# windows-unit-tests.ps1, windows-integration-tests.ps1, and windows-build-adp.ps1.
#
# The functions here are intentionally narrow and free of saluki-specific knowledge so each
# script can layer its own build/test/package logic on top. Import via:
#
#   Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force
#
# `-Force` matters: GitLab caches the docker layer with this module embedded, and a stale
# already-loaded module would otherwise mask edits during iteration.

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
        [string]$ExpectedSha256,

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

        $ActualSha256 = (Get-FileHash -Algorithm SHA256 -Path $Archive).Hash.ToLowerInvariant()
        if ($ActualSha256 -ne $ExpectedSha256.ToLowerInvariant()) {
            throw "protoc archive checksum mismatch: expected ${ExpectedSha256}, got ${ActualSha256}"
        }

        Expand-Archive -Path $Archive -DestinationPath $ExtractRoot -Force
        Copy-Item -Recurse -Force (Join-Path $ExtractRoot "*") $InstallRoot
    }

    $env:PROTOC = $ProtocBin
    Add-PathEntry (Join-Path $InstallRoot "bin")
    Invoke-Native $ProtocBin --version
}

function Initialize-RustEnvironment {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepoRoot
    )

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
    if (-not $env:PROTOC_VERSION) {
        $env:PROTOC_VERSION = "29.3"
    }
    if (-not $env:PROTOC_SHA256) {
        $env:PROTOC_SHA256 = "57ea59e9f551ad8d71ffaa9b5cfbe0ca1f4e720972a1db7ec2d12ab44bff9383"
    }
    if (-not $env:WINDOWS_CI_PROTOC_HOME) {
        $env:WINDOWS_CI_PROTOC_HOME = Join-Path $RepoRoot ".ci-cache\protoc\$env:PROTOC_VERSION"
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
        throw "Rust toolchain not found in Windows build image. Install Rust in the image instead of downloading rustup at job runtime."
    }
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
        throw "rustup not found in Windows build image. Install rustup in the image instead of downloading it at job runtime."
    }

    $ToolchainMatch = Select-String -Path (Join-Path $RepoRoot "rust-toolchain.toml") -Pattern 'channel\s*=\s*"([^"]+)"' | Select-Object -First 1
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
    Ensure-Protoc -Version $env:PROTOC_VERSION -ExpectedSha256 $env:PROTOC_SHA256 -InstallRoot $env:WINDOWS_CI_PROTOC_HOME
}

Export-ModuleMember -Function Invoke-Native, Add-PathEntry, Ensure-Protoc, Initialize-RustEnvironment
