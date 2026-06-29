# Builds agent-data-plane.exe for Windows and packages it into a release zip. Driven by
# .gitlab/windows.yml's `build-release-zip-windows-amd64[-fips]` jobs; the resulting zip is
# promoted to s3 by `push-release-zip-windows-amd64[-fips]` in .gitlab/release.yml.
#
# All inputs come from the environment so the GitLab job can pass them via `docker run -e`:
#
#   BUILD_PROFILE     Cargo profile (release on dev pipelines, optimized-release on tagged).
#   BUILD_FEATURES    Cargo features list (default | fips).
#   ADP_VERSION       Version slug for the zip filename (CI sets to ${ADP_IMAGE_VERSION}).
#   TARGET_ARCH       amd64 (only Windows arch in scope today).
#   OUTPUT_DIR        Directory under c:\mnt where the final zip lands so GitLab `artifacts:`
#                     can pick it up. Job sets this to the repo root.
#
# The cargo build itself mirrors the `make build-adp-host` Makefile target's APP_* env wiring
# so saluki-metadata renders the same metadata strings as the linux/darwin binaries.

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

foreach ($var in @("BUILD_PROFILE", "BUILD_FEATURES", "ADP_VERSION", "TARGET_ARCH", "OUTPUT_DIR")) {
    if (-not (Get-Item "env:$var" -ErrorAction SilentlyContinue)) {
        throw "windows-build-adp: required env var '$var' is not set"
    }
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

Initialize-RustEnvironment -RepoRoot $RepoRoot

# Install cargo-auditable so the `cargo auditable build` below embeds an SBOM (the dependency tree)
# into the binary, matching the linux/darwin CI builds. Installed the same way as protoc/NASM/LLVM:
# a pinned version + SHA256, cached under .ci-cache (see .gitlab/windows.yml cache:paths). Keep the
# version in sync with CARGO_TOOL_VERSION_cargo-auditable in the Makefile. The windows release zip is
# flat -- cargo-auditable.exe sits at the archive root -- so BinSubdir is empty.
$CargoAuditableVersion = "0.7.5"
$CargoAuditableSha256 = "83a7d5955c7ac96ede5d896ac9ede5f7ecce9ece0e95d9e47acd766b09e2ef1b"
Install-CachedZipTool `
    -Url "https://github.com/rust-secure-code/cargo-auditable/releases/download/v$CargoAuditableVersion/cargo-auditable-x86_64-pc-windows-msvc.zip" `
    -ExpectedSha256 $CargoAuditableSha256 `
    -InstallRoot (Join-Path $RepoRoot ".ci-cache\cargo-auditable\$CargoAuditableVersion") `
    -BinSubdir "" `
    -BinaryName "cargo-auditable.exe"

if ($env:BUILD_FEATURES -eq "fips") {
    # aws-lc-fips-sys (pulled in by --features fips -> rustls/fips) needs NASM, libclang,
    # and perl exposed -- see Initialize-FipsBuildTools for the full story.
    Initialize-FipsBuildTools -RepoRoot $RepoRoot
    # aws-lc-fips-sys's CMake builder calls its own printenv.bat which hard-codes the VS
    # search to %ProgramFiles(x86)%\Microsoft Visual Studio (and vswhere there), neither of
    # which resolve to the buildimage's actual install at c:\devtools\vstudio. The junction
    # makes the script's default-path search find it; the script itself then runs vcvarsall
    # internally so we don't need to activate the MSVC environment in our session.
    New-VsBuildToolsJunction
}

# saluki-metadata reads these at build time. Must match the values the Makefile passes through
# (ADP_APP_FULL_NAME / ADP_APP_SHORT_NAME / ADP_APP_IDENTIFIER / ADP_APP_GIT_HASH /
# ADP_APP_VERSION / ADP_APP_BUILD_DATE in Makefile) so the Windows binary identifies itself
# the same way as the linux/darwin binaries do.
$env:APP_FULL_NAME = "Agent Data Plane"
$env:APP_SHORT_NAME = "data-plane"
$env:APP_IDENTIFIER = "agent-data-plane"
$env:APP_VERSION = $env:ADP_VERSION
# Windows PowerShell 5.1 (the default `powershell.exe` in the LTSC2022 build image) doesn't
# have Get-Date's -AsUTC switch (added in PS 7.1). ToUniversalTime() works on both.
$env:APP_BUILD_DATE = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
if (-not $env:APP_GIT_HASH) {
    $env:APP_GIT_HASH = if ($env:CI_COMMIT_SHA) {
        $env:CI_COMMIT_SHA.Substring(0, [Math]::Min(7, $env:CI_COMMIT_SHA.Length))
    } else {
        try { (git rev-parse --short HEAD) } catch { "not-in-git" }
    }
}

Write-Host "[*] Building agent-data-plane (profile=$env:BUILD_PROFILE features=$env:BUILD_FEATURES)..."
Invoke-Native cargo auditable build --profile $env:BUILD_PROFILE --bin agent-data-plane --features $env:BUILD_FEATURES
Assert-NoDynamicVCRuntimeImports -BinaryPath (Join-Path $env:CARGO_TARGET_DIR "$env:BUILD_PROFILE\agent-data-plane.exe")

Write-Host "[*] Packaging Windows release zip..."
& (Join-Path $PSScriptRoot "package-adp-zip.ps1")
if ($LASTEXITCODE -ne 0) {
    throw "package-adp-zip.ps1 failed with exit code $LASTEXITCODE"
}
