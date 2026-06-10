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

if ($env:BUILD_FEATURES -eq "fips") {
    # aws-lc-fips-sys (pulled in by --features fips -> rustls/fips) needs NASM + Go + Ninja
    # + LLVM/libclang on Windows. The LTSC2022 buildimage doesn't ship them; install at job
    # runtime, cached under .ci-cache\ between runs.
    Initialize-FipsBuildTools -RepoRoot $RepoRoot
    # aws-lc-fips-sys's CMake builder calls its own printenv.bat which hard-codes the VS
    # search to %ProgramFiles(x86)%\Microsoft Visual Studio (and vswhere there), neither of
    # which resolve to the buildimage's actual install at c:\devtools\vstudio. Create a
    # directory junction so the script's default-path search succeeds, then activate the
    # MSVC environment in our own session for any tooling that reads it from env.
    New-VsBuildToolsJunction
    Initialize-MsvcEnvironment
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
Invoke-Native cargo build --profile $env:BUILD_PROFILE --bin agent-data-plane --features $env:BUILD_FEATURES

Write-Host "[*] Packaging Windows release zip..."
& (Join-Path $PSScriptRoot "package-adp-zip.ps1")
if ($LASTEXITCODE -ne 0) {
    throw "package-adp-zip.ps1 failed with exit code $LASTEXITCODE"
}
