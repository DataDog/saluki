# Assembles a release zip for an already-built agent-data-plane.exe. The Windows analogue of
# ci/tooling/package-adp-tarball.sh; both feed the same downstream consumer (the Datadog Agent
# omnibus build's datadog-agent-data-plane software definition), but the on-disk layout and
# extension differ to match Windows conventions:
#
#   bin\agent-data-plane.exe
#   LICENSE
#   LICENSE-3rdparty.csv
#   NOTICE
#   LICENSES\THIRD-PARTY-*
#
# All inputs are read from the environment (see the validation block below). Driven by
# ci/tooling/windows-build-adp.ps1; can also be invoked directly from the saluki repo root
# after building agent-data-plane.exe.

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

foreach ($var in @("OUTPUT_DIR", "BUILD_PROFILE", "TARGET_ARCH", "ADP_VERSION")) {
    if (-not (Get-Item "env:$var" -ErrorAction SilentlyContinue)) {
        throw "package-adp-zip: required env var '$var' is not set"
    }
}

# SPDX license-list-data tag used to harvest THIRD-PARTY-* license texts. Pinned to match
# ADP_SPDX_LICENSES_VERSION in Makefile and docker/Dockerfile.agent-data-plane so the
# Windows zip ships the same license texts as the linux/darwin tarballs; bump in lockstep.
$SpdxLicensesVersion = "3.28.0"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

$CargoTargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $RepoRoot "target" }
$Binary = Join-Path $CargoTargetDir "$env:BUILD_PROFILE\agent-data-plane.exe"
if (-not (Test-Path $Binary)) {
    throw "package-adp-zip: expected binary at '$Binary' (run windows-build-adp.ps1 first)"
}
foreach ($f in @("NOTICE", "LICENSE", "LICENSE-3rdparty.csv")) {
    if (-not (Test-Path $f)) {
        throw "package-adp-zip: missing '$f' in CWD ($RepoRoot)"
    }
}

$FipsSuffix = if ($env:BUILD_FEATURES -eq "fips") { "-fips" } else { "" }
$ZipName = "agent-data-plane-$($env:ADP_VERSION)-windows-$($env:TARGET_ARCH)$FipsSuffix.zip"
Write-Host "[*] Packaging $ZipName"

# Stage everything under a fresh per-invocation directory so re-runs don't pick up stale files.
$Stage = Join-Path ([System.IO.Path]::GetTempPath()) ("saluki-adp-zip-" + [System.Guid]::NewGuid().ToString("N"))
try {
    $StageRoot = Join-Path $Stage "zip"
    New-Item -ItemType Directory -Force (Join-Path $StageRoot "bin") | Out-Null
    New-Item -ItemType Directory -Force (Join-Path $StageRoot "LICENSES") | Out-Null

    Copy-Item -Force $Binary (Join-Path $StageRoot "bin\agent-data-plane.exe")
    foreach ($f in @("NOTICE", "LICENSE", "LICENSE-3rdparty.csv")) {
        Copy-Item -Force $f (Join-Path $StageRoot $f)
    }

    # Replicate the `license-builder` stage from docker/Dockerfile.agent-data-plane on the host so
    # the zip ships the same per-license THIRD-PARTY-* files as the linux/darwin artifacts. The
    # SPDX-id filter mirrors the awk/grep/sed pipeline in package-adp-tarball.sh.
    Write-Host "[*] Fetching SPDX license-list-data v$SpdxLicensesVersion"
    $SpdxArchive = Join-Path $Stage "spdx.tar.gz"
    Invoke-WebRequest -UseBasicParsing `
        -Uri "https://github.com/spdx/license-list-data/archive/refs/tags/v$SpdxLicensesVersion.tar.gz" `
        -OutFile $SpdxArchive
    # `tar.exe` ships with Windows 10/Server 2019+ and is present in the LTSC2022 build image.
    Invoke-Native tar -C $Stage -xzf $SpdxArchive
    $SpdxTextDir = Join-Path $Stage "license-list-data-$SpdxLicensesVersion\text"

    Write-Host "[*] Harvesting THIRD-PARTY-* license texts"
    $SkipTokens = @("OR", "AND", "WITH", "Custom", "LLVM-exception")
    $SpdxIds = Get-Content "LICENSE-3rdparty.csv" |
        Select-Object -Skip 1 |
        ForEach-Object { ($_ -split ",")[2] } |
        Where-Object { $_ } |
        ForEach-Object { $_ -split '\s+' } |
        Where-Object { $_ -and ($SkipTokens -notcontains $_) } |
        ForEach-Object { $_ -replace '[()]', '' } |
        Where-Object { $_ } |
        Sort-Object -Unique

    foreach ($Id in $SpdxIds) {
        $Source = Join-Path $SpdxTextDir "$Id.txt"
        if (-not (Test-Path $Source)) {
            throw "package-adp-zip: SPDX text not found for id '$Id' at '$Source'"
        }
        Copy-Item -Force $Source (Join-Path $StageRoot "LICENSES\THIRD-PARTY-$Id")
    }

    New-Item -ItemType Directory -Force $env:OUTPUT_DIR | Out-Null
    $OutputPath = Join-Path $env:OUTPUT_DIR $ZipName
    if (Test-Path $OutputPath) { Remove-Item -Force $OutputPath }
    # Compress-Archive can be slow with many small files but is sufficient here (~hundreds of
    # license texts plus one binary). Switch to [IO.Compression.ZipFile]::CreateFromDirectory if
    # this becomes a bottleneck.
    Compress-Archive -Path (Join-Path $StageRoot "*") -DestinationPath $OutputPath -CompressionLevel Optimal

    Write-Host "[*] Zip ready"
    $Hash = (Get-FileHash -Algorithm SHA256 -Path $OutputPath).Hash.ToLowerInvariant()
    Write-Host "$Hash  $OutputPath"
} finally {
    if (Test-Path $Stage) {
        Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
    }
}
